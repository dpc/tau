# Security policy

Tau is still early-stage software, but security issues are important. Please
report suspected vulnerabilities through GitHub private vulnerability reporting
for `dpc/tau` (`https://github.com/dpc/tau/security/advisories/new`) when
available. If that path is unavailable, contact the maintainer privately first
and avoid filing a public issue with exploit details.

## Harness and extension boundaries

The harness treats extensions as less-trusted peers connected over the Tau
protocol. For extension-owned persistent data, the harness confines paths to
per-extension state roots, rejects path traversal and symlink escapes, uses
private file and directory permissions where supported, and enforces per-file and
per-directory-list quotas. Quota failures are returned to extensions as
`quota_exceeded` extension-data errors.

Event subscriptions are also a data-exposure and resource boundary. Peers should
subscribe to exact event names by default so new protocol events do not
silently expand live delivery, replay catch-up, high-volume traffic, or access to
sensitive/contentful payloads. Prefix/category subscriptions should be reserved
for intentionally generic observers that truly need the entire category; changes
to subscribers must consider replay behavior, payload size/frequency, and whether
the selected events carry prompt, tool, provider, or extension-provided content.

These quotas bound individual file writes, file reads, and directory listing
work performed by the harness. They do not bound aggregate per-extension disk
usage across many files, sandbox arbitrary extension code, or prevent protocol
payloads from being deserialized before the harness validates an operation. Run
only extensions you trust to execute on your machine.

Cross-harness agent messages are local IPC between Tau harness daemons for the
same user. They use a dedicated external-message RPC rather than generic event
`emit`; extensions and ordinary UI clients cannot publish harness-owned
`agent.message_sent` / `agent.message_received` projections directly. The
receiving harness validates its active target session and recipient state before
recording an inbound projection. Runtime daemon metadata is discovery data only:
`session_id` means the daemon's current active session and is updated on
`/session new`; stale or ambiguous metadata must fail discovery rather than
silently choosing a target. A failed socket probe alone must not delete runtime
discovery files while the metadata pid is still live; dead-pid entries remain
eligible for cleanup on platforms where Tau has a safe pid-liveness backend.

`extensions.<name>.require: false` is only a degraded-startup availability
policy for trusted local extensions. It lets Tau continue without that extension
when startup/config/secret/pre-Ready setup fails; it is not a sandbox and does
not broadly change post-Ready respawn/runtime semantics. Optional skips must
still be surfaced as mandatory replayable `harness.notice` diagnostics and must
never hide extension config errors or leak secret values. Notice filtering is a
UI-side preference: critical and mandatory warning notices remain visible,
replayable, and protected from interceptor rewrite/drop. Extension-authored
notices are sanitized so extensions cannot spoof critical or always-visible
harness diagnostics.

Per-agent metadata (`agent.metadata_set` / `agent.metadata_unset`) is durable,
extension-visible, and interceptable by privileged local interceptors. It is a
coordination mechanism, not a secret store: do not put API keys, tokens, private
message contents, or other confidential data in metadata. Key ownership is by
convention (for example `ext_<extension-instance>_cwd`); trusted extensions, UIs,
and interceptors that can emit protocol events can attempt to write any metadata
key subject to harness validation.

## PIM extension

`std-pim` / `tau-ext-pim` is disabled by default because it bridges external
email and calendar providers into Tau. Email messages, calendar events, folder
names, calendar names, MIME headers, provider errors, ICS feeds, and Google API
responses are untrusted data and can contain prompt injection, terminal control
bytes, spoofing content, or huge payloads.

PIM credentials are local secrets. Mail passwords, app passwords, OAuth client
secrets, OAuth refresh tokens, access tokens, pending device codes, PKCE
verifiers, and private ICS URLs must come from Tau secrets or private extension
state and must not be placed in model-visible output, action output, audit logs,
tracing, notices, prompt history, debug logs, or config examples. Pasted
authorization-code redirect URLs are transient sensitive user input because they
contain one-time authorization codes; they likewise must not be persisted or
echoed. Google Calendar OAuth actions may display only the verification URL and
user code. Google Gmail OAuth actions may display the installed-app
authorization URL, but must not echo the pasted redirect URL, authorization
code, PKCE verifier, refresh token, or access token. Accounts configured with
manual `refresh_token_secret` values must refuse state-owned OAuth actions so
the extension does not overwrite secret ownership.

Incoming email body reads are fail-closed behind policy or exact user approval.
The extension may consume trusted provider-added `Authentication-Results`
metadata for sender policy, but it does not cryptographically verify DKIM
itself and raw authentication headers are not model-visible. Outgoing email
approval must avoid partial sends, stale approvals, and duplicate sends. In
particular, OAuth retry may retry authentication before SMTP message submission,
but must not retry an entire message after SMTP submission may have begun.

Calendar writes should require approval by default, keep provider concurrency
tokens such as Google ETags internal, and avoid exposing private event details
unless policy allows them. Email folder ids and calendar ids are model-visible
opaque ids returned by list tools; do not document or expose internal encoding
details in prompts, tool descriptions, docs, or self-knowledge intended for the
model.

## Websearch extension

`std-websearch` / `tau-ext-websearch` is enabled by default and sends model tool
arguments to external hosted MCP web providers. Treat provider responses as
untrusted web content that can contain prompt injection, misleading text, or large
payloads. The extension must keep successful response bodies, decoded
model-visible output, and concurrency bounded.

Endpoint override URLs are configuration but may still contain secrets in
userinfo, query strings, or fragments. The extension must not log raw endpoint
override URLs, must reject URL userinfo credentials and unsupported auth forms,
and must not send Parallel Authorization headers. Production provider endpoints
must use HTTPS; plaintext HTTP is only acceptable for loopback test endpoints.
Provider transport diagnostics and JSON-RPC errors can be surfaced as
model-visible tool errors, so configured endpoint echoes, request targets, query
keys/values, fragments, and userinfo must be sanitized and finally bounded before
return.

## Core shell extension

`std-shell` / `tau-ext-shell` is Tau's local filesystem and subprocess boundary. Its tools can read local files, mutate files, and execute host commands with the user's permissions. Treat shell commands, user `!` commands, and model-requested filesystem writes as local code/data access rather than sandboxed operations.

Shell timeout and cancellation paths terminate the spawned Unix process group
where supported, but descendants that deliberately detach into another process
group or session may survive. The shell extension therefore must not treat
stdout/stderr pipe EOF as a required completion signal after foreground exit,
timeout, or cancellation; it should perform only bounded output draining before
returning.

Read-only shell mode is a defense-in-depth feature of the opt-in directory-lock mechanism. When `dir_lock.enable` is true, shell tool calls are inferred read-only unless the calling agent holds a manual lock covering the command cwd; `dir_lock.enforce_ro_bind` controls whether inferred read-only calls also require a native read-only bind mount. With the default `enforce_ro_bind: true`, unsupported or failed native isolation makes the shell call fail closed. If users explicitly set `enforce_ro_bind: false`, inferred read-only shell execution degrades to ordinary command execution and must not be treated as a hard sandbox. Directory update locks are advisory coordination between Tau agents and ext-shell tools, not an operating-system access-control boundary. They do not prevent commands from writing outside their locked working directory or other local processes from changing files. The filesystem directory-lock backend coordinates multiple Tau/ext-shell instances for the same host and user through a private local registry, but its leases end when ext-shell exits; detached shell descendants can keep mutating files after the lease is gone. The shell extension remembers each agent's current cwd in durable metadata (`ext_<extension-instance>_cwd`); that value is visible to extensions and should be treated as non-secret path context.

Tool and model tags are prompt-surface/routing metadata, not a sandbox. Extensions publish neutral tool tags, providers publish model tags, and the harness owns matching policy plus prompt-time tool snapshots. A provider tool call is authorized against the snapshot advertised to that prompt, not against later role/model changes. Role `disable_tools` and unpinned shell/edit alternative suppression are policy controls for the model-visible surface; they do not prevent trusted local extensions or host processes from accessing the filesystem outside Tau's tool route.

Model-visible diagnostics are part of the prompt surface. Keep tool rejection,
schema validation, and path suggestion text deterministic and bounded so
extension-provided schemas, filesystem names, or model arguments cannot amplify
unbounded work or prompt content.

AGENTS.md files are trusted implicit prompt input loaded during session startup.
Tau follows AGENTS.md symlinks in both user and project roots. Do not run Tau in
repositories whose instructions you do not trust; a malicious AGENTS.md or skill
can steer the agent just like any other trusted prompt input. Following these
symlinks is an intentional accepted behavior for linked instruction layouts, not
a sandbox boundary.

Failure-triggered tool examples are diagnostic metadata in the same prompt-surface
class. Providers may attach compact examples to tool registrations, but the
harness must validate them at registration, reject invalid examples visibly, omit
them from normal provider tool definitions, and surface at most bounded example
text after a failed call.

Schema-guided argument repair is a local pre-dispatch convenience, not a trust
boundary. Keep repairs narrow, run them only after validation failure, revalidate
before dispatch, keep repair traces bounded and metadata-only, and preserve
deterministic rejection diagnostics when repair does not apply.

Loop-guard prompts are harness-authored internal steering. Keep loop signatures
compact and runtime-only, inject at most one breaker for a detected cycle, and
surface a harness notice instead of creating unbounded self-dialogue when the
cycle continues.

## Skills

Skills are prompt instructions loaded from local/project Markdown files, not a sandbox or permission boundary. Project skills can be malicious prompt content. `disable-model-invocation` hides a skill from Tau's model-visible skill surfaces, but a model with filesystem tools could still read the underlying file if it learns the path. `allowed-tools` and similar frontmatter fields do not grant or restrict Tau tool permissions.

Skill discovery is best-effort and bounded. It reads only a bounded
frontmatter prefix during session startup, skips and diagnoses skills whose
frontmatter does not close inside that prefix, and bounds directory traversal by
explicit per-root, per-directory, and depth budgets. Exceeding a traversal budget
emits a diagnostic and skips the remaining over-budget traversal. Symlinked
skill roots and entries are followed while tracking canonical directories to
avoid recursion cycles. Project-controlled skill roots can therefore point
discovery at other local skill files reachable by the user; do not treat skill
discovery as a sandbox.

`agents.required_skills` and group/role `required_skills` are fail-closed
availability checks, not permission boundaries. They only require that exact
skill names are discoverable and model-loadable before a role may be selected or
delegated; they do not make skill content trusted, restrict filesystem access to
the skill file, or grant tools mentioned by skill frontmatter.

User `/skill` invocation explicitly reads the selected skill file, strips frontmatter, and injects the skill body into the next model prompt along with any user arguments. Treat invoking a skill as intentionally adding that local file content to the conversation context.

## Telegram extension

`std-telegram` / `tau-ext-telegram` is disabled by default because it bridges
untrusted external Telegram text into Tau prompts. When enabled, it requires an
explicit bot-token secret and non-empty allowlist of Telegram user ids. The model
cannot provide arbitrary chat ids: outgoing messages use only the configured
chat or an allowlisted user's linked private chat. Unconfigured group/supergroup
chats are refused, and configured groups should be treated as shared output
channels visible to everyone in that chat. Runtime registrations, selected
agents, learned chat id, and update offsets are in-memory only. Avoid logs that
include bot tokens, Bot API URLs, or unexpected private Telegram content.

## XMPP extension

`std-xmpp` / `tau-ext-xmpp` is disabled by default because it bridges
untrusted external XMPP text into Tau prompts. When enabled, it requires an
explicit password secret, a non-empty `allowed_jids` allowlist, and a
`default_recipient` that matches that allowlist. The model cannot choose
arbitrary destination JIDs: outgoing messages use only the registered agent
conversation, and MUC invites/notices go only to the configured allowlisted
default recipient. Outbound MUC messages are visible to room occupants. The MVP is
plaintext XMPP over TLS only and does not implement OMEMO/E2EE, so XMPP
servers/operators can read message content. Tau submits only the XEP-0045
instant-room owner form needed to unlock newly-created MUC rooms; it does not
configure MUC privacy or member affiliations itself. Private/hidden/members-only policy
must come from server defaults or preconfiguration. MUC mode must verify real
sender JIDs from room presence by default; if a server hides real JIDs, the
extension fails closed unless the user explicitly configures trust in
server-side room membership. Runtime registrations and room mappings are
in-memory only; MUC room localparts use short readable session/agent slugs plus a
compact lowercase-base32, domain-separated BLAKE3 disambiguator over the full Tau
session id and validated agent id. The readable slugs are not authoritative for
routing, and the intentionally short disambiguator is not injective: if generated
rooms ever collide in-process after XMPP JID normalization, registration must
fail closed instead of overwriting an existing room mapping. Successfully joined
rooms must remain tracked until leave/unavailable presence can be sent or the
connection is gone. Avoid logs that include XMPP passwords or private message
content.

## Rhai scripting extension

`std-rhai` / `tau-ext-rhai` scripts are trusted local code. A Rhai script can
register agent-invokable tools, handle model-originated tool calls, emit raw Tau
events, and execute host shell commands directly through the Rhai extension.
These shell commands intentionally do not route through `tau-ext-shell` and do
not participate in ext-shell directory-update locks; only enable scripts you
would be comfortable running as local programs. On Unix, Rhai shell commands use
an owned process group/session for timeout and shutdown cleanup, but commands can
detach descendants into another process group/session while keeping stdout/stderr
inherited. Those descendants may survive the owned process-group kill, so Rhai
shell output capture after foreground completion, timeout, or cancellation is
bounded and must not wait indefinitely for pipe EOF.

## Interception boundary

Interceptors are privileged local extensions. They can see, modify, or drop most
events they subscribe to before those events commit. Must-pass and immutable
checks protect selected harness-owned facts from integrity loss, but they are not
confidentiality boundaries: do not expose sensitive event streams to interceptors
you do not trust.

## CLI terminal UI

The terminal UI executes trusted local configuration and environment-derived
commands, including key-binding shell snippets, completion commands, `$EDITOR`,
and `$VISUAL`. Treat `cli.yaml`, inherited environment variables, and PATH as
local code execution inputs rather than untrusted data.

Prompt completion may read the local filesystem and query `git` for tracked and
unignored files. These operations should stay bounded and best-effort: failures
or quota/size limits should disable the completion source or surface a local
notice, not wedge the prompt.

Theme completion and no-argument `/theme` listings may inspect custom theme
files only for optional display metadata. These reads must remain best-effort
and bounded: avoid opening non-regular or special theme directory entries, do
not follow symlinks in the metadata path, keep a byte limit for regular files in
case of races, and list malformed, oversized, unreadable, or special entries by
name with an empty description instead of blocking or failing the prompt.

The hidden `tau dev tmux` helper is trusted local testing infrastructure, not a
sandbox. It starts Tau under scratch HOME/XDG paths to avoid accidental config
or state writes during manual E2E checks, but it still runs local processes with
the user's permissions. Scratch cleanup must remain guarded by a helper marker
and path validation so `--remove-scratch` cannot recursively delete arbitrary
user directories. Target commands such as capture, send, and stop must validate
the recognized helper marker and scratch-root shape before connecting to a tmux
socket, and cleanup must validate that ownership before killing a session or
removing the scratch root.

Provider credentials for `tau dev tmux start` are local-only by default. The
helper must not copy provider profiles, tokens, API keys, provider config, or
provider state from the user's real Tau directories unless the user explicitly
opts in through `testing.yaml`. That allowlist names exact provider profile
names only; the helper may copy only the corresponding
`auth.d/<provider>.json` files into scratch state, must not copy lock files,
general config, sessions, logs, unrelated provider profiles, whole directories,
or "all providers", and must refuse symlink/path-traversal attempts around those
files. Reused scratch destinations must be reconciled to the current allowlist
and must not write through pre-existing symlinks, non-regular files, or
externally linked entries. Missing or empty testing configuration must be
surfaced as a warning and must continue with no provider credentials in the
scratch environment.

Raw terminal mode is a process-local ownership boundary. Before spawning editors
or pickers, Tau must pause redraws, release raw-mode features, and always clear
that paused state when setup or resume fails so the UI cannot remain permanently
muted. Abort paths for terminal-releasing shell actions should terminate the
owned process group before Tau resumes raw-mode/redraw ownership. Redraw and
input coordination assumes a single foreground reader thread; background
renderer threads must not write while the terminal is released to an external
program.

External-editor prompt trailers are prompt-surface text. They may quote
assistant responses and prior prompt text to help compose the next prompt, but
the terminal UI must scope response context to the currently visible/no-agent
transcript and must not let hidden-agent rendering publish a different agent's
response into the shared editor context.

Transcript Markdown-lite formatting is a presentation-only terminal UI feature.
It must not change protocol events, persisted logs, model context, or non-UI
clients, and it must produce only Tau styled text spans rather than raw terminal
escape sequences. Keep its scope narrow to submitted user prompts, assistant
responses, and thinking text; do not accidentally run it over tool output, shell
output, or other machine-generated blocks where styling could obscure exact
results. Markdown table padding is also display-only: it may add spacing around
cell contents for readability, but must preserve the cell text, avoid code
contexts, and keep bounded output amplification.

The CLI `redraw_history_size` setting bounds only how many already-rendered
history rows the terminal UI replays to stdout when rebuilding Tau-owned
scrollback after a full redraw. It does not truncate in-memory UI state,
protocol events, durable session logs, provider/model context, or any other
non-terminal history.

`tau --ephemeral` is a session-persistence mode, not a privacy sandbox. It
prevents the current harness process from writing session membership logs,
session metadata/locks, per-session debug `events.jsonl`, per-session
harness/extension stderr logs, session-scoped extension data, and terminal UI
logs. Agent transcripts remain durable under the global agent store unless an
agent is explicitly staged as ephemeral with `/new` then `/ephemeral on`.

Ephemeral agents are also local Tau persistence controls, not confidentiality
boundaries. Their own semantic transcript, metadata, durable session membership,
ephemeral-agent debug JSONL entries, and prompt-history rows stay memory-only
while the daemon lives, but durable recipients/parents may persist projected
messages or results, and provider state, credentials, user/cache extension data,
policy/config files, runtime sockets, external services, interceptors, and
trusted tools/extensions keep their normal persistence and filesystem access. Do
not use session or agent ephemerality as a guarantee that prompt contents, tool
results, or extension-observed data cannot be persisted elsewhere.

Future event kinds that carry agent prompts, provider output, tool payloads, or
extension-observed content must update the durable debug-log suppression rules
and regression tests before they are emitted for ephemeral agents.

## Reporting guidance

When reporting a vulnerability, include:

- affected Tau version or commit;
- operating system and relevant configuration;
- minimal reproduction steps;
- whether an extension, provider, UI client, or daemon boundary is involved;
- any logs that do not contain secrets.

Avoid sharing API keys, OAuth tokens, email contents, or other private data in
reports.


## Provider streaming trust boundary

Provider response progress updates are transient and untrusted. The harness validates the provider prompt owner and derives the published `agent_id` from harness state so a provider cannot route streaming deltas to another agent by forging ids. Provider-authored retry/status diagnostics must stay separate from assistant text deltas to avoid confusing diagnostics with model-authored transcript content; durable truth remains the final response event.

First-party providers also run a bounded exact streaming repetition guard before accepting assistant text, reasoning summaries, or tool-argument deltas. This is a resource-safety guard for tight provider/model loops, not a semantic quality filter: it only checks substantial exact suffix repetitions inside one stream component and clears transient output before publishing a `repetition_detected` final response.
