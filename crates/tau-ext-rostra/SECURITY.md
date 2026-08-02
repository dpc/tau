# tau-ext-rostra security boundary

`tau-ext-rostra` is a trusted same-user extension process. Its Rostra peer data
is untrusted external content even when a valid signature identifies its author.
Tools bound and sanitize every remote field and wrap multiline bodies as
external content. A Rostra identity never becomes Tau user, agent, or sender
authority.

The extension uses relay-only Iroh peer transport, with Pkarr HTTPS/DNS
discovery, and never enables direct peer-IP transport. It has no Rostra signing secret. Its
owner-private, exclusive redb contains the replicated public graph,
projections, acquisition metadata, and a persistent Iroh node secret. The
database survives sessions outside Tau journals. Do not share it with another
Rostra process.

Tool calls run outside the protocol reader under a ten-second model-visible
deadline and an eight-query admission cap. Cancellation and timeout suppress
the terminal result but cannot interrupt an upstream redb read, which uses
`block_in_place`; the scan retains its permit until it returns. Reconfiguration
is rejected while any query remains active. Filtered social scans have no
upstream processing bound even though output is bounded. Shutdown waits one
second, then relies on supervised-process termination to contain retained work.
A query panic is isolated to its task and becomes `internal_failure`;
synchronization-worker health remains unobservable and `rostra_status` reports
it as unknown rather than healthy. A DB-owning worker process is deferred unless
upstream adds bounded or cancellable scans.

Tool output is capped at 128 KiB, including bounded tag count and aggregate tag
bytes. The upstream store can still deserialize a full accepted Rostra payload
before Tau projects it; its 16 MiB ingestion limit is the remaining per-record
RAM cliff. Database open and upstream migration can also consume temporary disk.
Back up `rostra.redb` before upgrading. Schema migration is forward-only: stop
Tau, restore the backup, and run the previous version to roll back. Locked,
corrupt, or identity-mismatched stores fail closed; restart reopens the store
and starts fresh synchronization workers.

The initial interface has no writes, identity creation, key custody, direct-IP
mode, arbitrary on-demand synchronization, notifications, followee-listing,
inbound messages, or activation.
