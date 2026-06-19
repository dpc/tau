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

These quotas bound individual file writes, file reads, and directory listing
work performed by the harness. They do not bound aggregate per-extension disk
usage across many files, sandbox arbitrary extension code, or prevent protocol
payloads from being deserialized before the harness validates an operation. Run
only extensions you trust to execute on your machine.

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

## Core shell extension

`std-shell` / `tau-ext-shell` is Tau's local filesystem and subprocess boundary. Its tools can read local files, mutate files, and execute host commands with the user's permissions. Treat shell commands, user `!` commands, and model-requested filesystem writes as local code/data access rather than sandboxed operations.

Read-only shell mode is a defense-in-depth feature of the opt-in directory-lock mechanism. When `dir_lock.enable` is true, shell tool calls are inferred read-only unless the calling agent holds a manual lock covering the command cwd; `dir_lock.enforce_ro_bind` controls whether inferred read-only calls also attempt a native read-only bind mount where supported. Without that native isolation, inferred read-only shell execution can degrade to ordinary command execution and must not be treated as a hard sandbox. Directory update locks are advisory coordination between Tau agents and ext-shell tools, not an operating-system access-control boundary. They do not prevent commands from writing outside their locked working directory or other local processes from changing files. The shell extension remembers each agent's current cwd in durable metadata (`ext_<extension-instance>_cwd`); that value is visible to extensions and should be treated as non-secret path context.

Tool and model tags are prompt-surface/routing metadata, not a sandbox. Extensions publish neutral tool tags, providers publish model tags, and the harness owns matching policy plus prompt-time tool snapshots. A provider tool call is authorized against the snapshot advertised to that prompt, not against later role/model changes. Role `disable_tools` and unpinned shell/edit alternative suppression are policy controls for the model-visible surface; they do not prevent trusted local extensions or host processes from accessing the filesystem outside Tau's tool route.

## Skills

Skills are prompt instructions loaded from local/project Markdown files, not a sandbox or permission boundary. Project skills can be malicious prompt content. `disable-model-invocation` hides a skill from Tau's model-visible skill surfaces, but a model with filesystem tools could still read the underlying file if it learns the path. `allowed-tools` and similar frontmatter fields do not grant or restrict Tau tool permissions.

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
would be comfortable running as local programs.

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

## Reporting guidance

When reporting a vulnerability, include:

- affected Tau version or commit;
- operating system and relevant configuration;
- minimal reproduction steps;
- whether an extension, provider, UI client, or daemon boundary is involved;
- any logs that do not contain secrets.

Avoid sharing API keys, OAuth tokens, email contents, or other private data in
reports.
