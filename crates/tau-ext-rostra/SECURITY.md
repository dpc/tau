# tau-ext-rostra security boundary

`tau-ext-rostra` is a trusted same-user extension process. Its synchronized
Rostra fields are untrusted external content even when a valid signature
identifies their author. Tools bound and sanitize every remote field and wrap
multiline bodies as external content. A Rostra identity never becomes Tau user,
agent, or sender authority.

The extension uses relay-only Iroh peer transport with Pkarr HTTPS/DNS
discovery and never enables direct peer-IP transport. It owns an exclusive
owner-private redb containing the replicated graph, local signed events,
acquisition metadata, and a persistent Iroh node secret. Do not share it with
another Rostra process.

The extension receives a 24-word Rostra mnemonic only as a declared Tau secret
referenced by `identity_mnemonic_secret`; it derives the public identity and
does not accept a second public ID. Tau removes `TAU_SECRET_*` values before
child spawn and the mnemonic is absent from configuration, journals, tool
arguments/results, and logs. The parsed key remains in process memory for the
client lifetime and upstream active tasks retain copies. `RostraIdSecretKey` is
copyable and has no zeroization guarantee.

The generic resolver gives the environment value precedence but falls back to
`<tau-state>/secrets/rostra_identity_mnemonic.yaml`; this is Tau-managed
file-backed custody under the owner-private state root, not an env-only
guarantee. The extension itself never writes the mnemonic into `rostra.redb`.

The client remains read-only until the first signed tool call. Lazy activation
can create a signed node announcement and starts Pkarr publication and
head-merging tasks. Each signed post, reaction, follow, unfollow, text-profile
update, or vote commits locally first; background network publication is
best-effort. The ten-second deadline and cancellation suppress a terminal
result after Tau admits the call for publication, which happens after
validation, lane acquisition, and lazy activation but before the
operation-specific upstream call. The admitted call retains the write lane and
can reach a non-interruptible redb write. Before admission Tau aborts its task,
although lazy activation may already have made its signed node announcement.
There is no outbox, remote acknowledgement, or automatic retry: an admitted
unknown outcome is possibly signed, stored, and published.

One write mutex serializes local signed calls. It avoids avoidable local forks
but does not prevent other devices from creating forks, and the upstream head
merger can sign a merge event. The existing eight-read admission cap remains.
Before signing `rostra_post` or `rostra_react`, that same lane reserves one
entry in an in-memory monotonic-time rolling window. It deliberately remains a
best-effort local anti-spam guard: it resets on extension restart or
reconfiguration, has no durable counter, and does not count synchronized
same-identity events from other devices. The reservation is retained after
dispatch so a failed or uncertain write cannot immediately bypass the guard.
Follows, unfollows, profile updates, and votes do not consume it. A full window
returns bounded `rate_limited` text plus fixed structured
`retry_after_seconds` details.
Tool policy controls which roles receive signing capability. Tau has no
genuine human per-call confirmation; operators must grant these tools only to
roles trusted to make permanent external statements. `enable_tool_groups:
[rostra]` grants all four local-view reads, all six persistent signed writes,
and `rostra_notifications`; use exact tool-name policy instead when a role
needs a smaller surface.

Tool output is capped at 128 KiB, including bounded tag count and aggregate tag
bytes. The upstream store can still deserialize a full accepted Rostra payload
before Tau projects it; its 16 MiB ingestion limit remains a per-record RAM
cliff. Database open and migration can consume temporary disk. Back up
`rostra.redb` before upgrading. Schema migration is forward-only: stop Tau,
restore the backup, and run the previous version to roll back. Locked, corrupt,
or identity-mismatched stores fail closed.

`rostra_notifications` never treats a Rostra author as a Tau sender. An
agent must explicitly enable its own preference. The worker selects only
direct-followee posts matching their persona selector, excludes self posts and
historical syncs, and projects every selected field as bounded hostile external
content. It uses the bounded durable materialization feed to recover from lossy
broadcasts. A batch becomes eligible no sooner than 30 seconds after quiet and
at five minutes after it starts; canonical delivery can be delayed by normal
harness processing. It emits no more often than every five minutes per agent.
The latter bounds
canonical reports and Rostra-caused wakes, not model execution: normal harness
busy batching can coalesce or delay work. A report previews at most 32 posts
within 48 KiB and summarizes excess; all excess remains pull-queryable.
The identity-bound policy/checkpoint file uses a mode-0600 temporary file,
file sync, atomic rename, and parent-directory sync. If that final directory
sync fails after rename, the extension installs the visible candidate in memory,
returns failure, and poisons later notification mutations. It never silently
claims success or rolls memory behind the renamed file.

The interface has no identity creation/import/export, direct-IP mode, arbitrary
on-demand synchronization, attachments or avatars, news/shoutbox, raw event
construction, followee listing, arbitrary inbound messages, or activation
directly from remote content.
