# ARCH-tau-ext-rostra: Read-only Rostra extension

`tau-ext-rostra` is the disabled-by-default built-in `std-rostra` tool
extension. One extension instance owns one configured public Rostra identity.
It starts a full `rostra-client = 0.1.0` client after configuration, hard-codes
relay-only Iroh peer transport with Pkarr HTTPS/DNS discovery and no direct
peer-IP transport, and continuously synchronizes while the supervised
extension process remains alive. Dropping the client during disconnect or
process shutdown stops its background tasks. It never accepts a secret or
creates an identity.

The extension exclusively owns `<state_dir>/rostra.redb`, normally under
`<tau-state>/ext/<instance>/`. The owner-private store survives Tau sessions and
contains Rostra graph state, projections, synchronization metadata, and the
Iroh node secret. Tau does not copy it into session or agent journals. Missing
stable state fails startup rather than selecting Rostra's non-synchronizing
memory client. An existing database bound to another identity fails closed.
Operators must choose a distinct instance or move the old state. This exception
for large, independently replicated, subscriber-unsafe, history-independent
state follows
[GATE-event-log-first-extension-state](../../../specs/GATE-event-log-first-extension-state.md).
The exact interface and persistence semantics were approved under
[GATE-persistence-and-extension-interface-change-approval](../../../specs/GATE-persistence-and-extension-interface-change-approval.md).

The model interface consists of exactly four pull-only tools:
`rostra_status`, `rostra_list_posts`, `rostra_read_post`, and
`rostra_get_profile`. Lists expose following, locally known two-hop network, and
explicit-author timelines. Every answer describes only the configured
identity's synchronized local view; empty and `not_found_local` results never
claim global absence. Pages default to 20 and never exceed 50 records. List
excerpts stop near 240 Unicode scalar values, and detailed Djot stops at 64 KiB.
Versioned bounded cursors bind continuation to its timeline and author filter.
They carry the exact upstream `EventPaginationCursor` or
`NewsRankPaginationCursor`, so insertion and reranking do not restart paging.
Exact `rostra-client-db = 0.1.0` and `rostra-core = 0.1.0` dependencies expose
those public cursor and selector/content types; all client construction,
networking, and storage access still go through `rostra-client`.
Stable error categories are `invalid_argument`, `not_ready`,
`not_found_local`, `network_unavailable`, `storage_failure`, `timeout`,
`cancelled`, and `internal_failure`.

The protocol reader only validates and schedules work. Tool queries run as
independent panic-isolated tasks, with a ten-second model-visible deadline and
an eight-query admission cap. Upstream redb reads use `block_in_place`, so
timeout and cancellation suppress terminals but do not interrupt an in-flight
scan; its permit remains occupied until return. Reconfiguration is rejected
while queries remain active. Filtered social scans have no processing bound
despite bounded output. Shutdown waits one second before process supervision
provides the forced-termination boundary. A DB-owning worker process remains a
follow-up only if upstream cannot provide bounded or cancellable scans.
Synchronization worker health is not exposed upstream and is reported as
unknown. Restart drops the client before its runtime, reopens the database, and
starts new workers. Locked, corrupt, and identity-mismatched databases fail
closed. Upstream schema migrations are forward-only; operators must stop Tau
and back up the database before upgrade, then restore it and the old binary to
roll back.

The trusted extension process receives hostile Rostra content. It bounds and
sanitizes names, bios, persona tags, post bodies, avatar metadata, and
diagnostics; multiline bodies use an external-content wrapper whose closing
sentinel cannot be forged. Signatures authenticate only the Rostra author.
Every remote field is inside an external-content frame. Final output is capped
at 128 KiB; tags also have count and aggregate-byte caps. Rostra can deserialize
a full accepted payload (up to its 16 MiB ingestion limit) before projection,
and open/migration can require temporary disk, so these upstream RAM and disk
cliffs remain operator-visible.
Rostra identities never acquire Tau sender or instruction authority.
Synchronization updates only the extension database.

Excluded contract boundaries are all Rostra writes; identity creation and key
custody; notification and followee-listing tools; arbitrary on-demand sync;
shared databases; direct-IP public mode; multiple identities per instance;
attachments and avatar delivery; inbound messages, prompt injection,
notifications, activation, or background model work. These require another
explicit interface and persistence decision rather than an incremental tool
addition.
