# SPEC-tau-harness-peer-discovery: Bounded peer and local-agent discovery

`session_list({ query?, limit? })` is disabled by default in the
`session_discovery` tool group. It scans bounded runtime candidates and metadata
bytes with bounded probe concurrency and deadlines, then returns only live
active sessions whose target harness accepts bare inter-session messages.
The scan visits at most 128 directory entries before any filename filtering,
reads at most 16 KiB from each accepted metadata candidate, and uses at most
eight process-wide probe slots. Each probe is limited to 250 ms and all scan,
metadata, connection, send, and response work shares one two-second absolute
deadline. Deadline cancellation stops further dequeue and I/O, and a call
joins its scoped workers before returning. At most eight top-level discovery
calls run process-wide; excess calls fail before spawning a worker. Potentially
blocking runtime-directory traversal and regular-file metadata reads run in an
isolated bounded worker. A stalled storage worker may outlive caller timeout,
but retains its global call lease until it exits, so repeated stalls cannot grow
threads or retained work without bound.
Results contain the session routing key, basename-only project label, and
current-session flag. They sort by session id; result and scan truncation are
explicit. Malformed, stale, unconfirmed, or ambiguous candidates are omitted.
The query is a bounded case-insensitive literal match, never a regex or glob.

`agent_list({ query?, role?, group?, state?, limit? })` is disabled by default in
the independent `agent_discovery` group. It reads current-harness state only and
has no cross-session enumeration RPC. Results contain agent id, immutable
creation role and group, `pending|idle|running`, and self flag. They exclude
task/display text, prompts, responses, cwd, tools, watcher topology, models, and
provider state. Filters and result count are bounded; exact role/group/state and
literal id-query matching are deterministic but snapshots remain racy.

Possessing either discovery tool does not grant messaging, watching, starting,
or compaction authority. Sending always revalidates its selected destination.
