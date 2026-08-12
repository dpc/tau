# tau-ext-slack security and reliability notes

Private reply, edit, and reaction state is installed only after the originating
configured publisher observes the exact canonical message fact on its live
post-commit downpath while installation and lifecycle remain current. Failed
submission and missing or mismatched echoes install nothing. Delete revokes local
authority immediately but keeps its report pending. A bounded process-local
native-id cache has no restart or cross-agent guarantee.

- `std-slack` is disabled by default. Enabling its tools is a role-policy decision; configuration is not a per-agent ACL.
- App/bot tokens and Socket Mode URLs are secrets. Never log them or message bodies; diagnostics stay bounded and token/URL-redacted. Production endpoints require HTTPS/WSS.
- Slack events, metadata, edits, reactions, and text are untrusted external ingress. ACK is transport behavior, not authorization. Model-facing bodies use `content_trust="external"` and may contain prompt injection.
- `auth.test` must establish one exact bot U/W plus installing T workspace pair at startup/reconnect and before a proactive send that has no live worker observation. Supported Events API wrappers must match that installation via exact `context_team_id` or one unambiguous authorization record; missing, mixed, malformed, or conflicting evidence is dropped before `users.info` or any local/ingress effect. Top-level event team data is not authority, and a Slack Connect actor's home team may differ.
- Mutable configuration replacement discards preflight installation evidence. Reconnect must exactly match the established bot/workspace pair, compared immediately after `auth.test` and before acquiring a socket ticket. A changed, incomplete, or malformed observation process-lifetime latches the extension offline, retires prior report submission, dynamic links/selections, reply/edit routes, post ownership, reaction authority, and workers, emits one bounded categorical restart notice, and requires restart. Later observations cannot reactivate it; old sends cannot retry or install private authority.
- The exact U/W id remains extension-local sender authority. Model context receives an installation-scoped opaque sender reference, bounded mutable display, and honest admission label; `sender_auth` grants no body trust, tool authority, or routing authority. An optional one-to-one operator `sender_aliases` binding is scoped by the harness-authenticated extension instance and has no admission/routing/reply/reaction/mention effect.
- Any exact installation-bot mention outside complete backtick code ranges participates in normalization. Exactly one eligible leading occurrence is removed for routing/command compatibility; remaining eligible occurrences normalize to the semantic `@slack_bridge` token in submitted report text. There is no separate generic mention field. The registration's advisory reference discloses no bot/workspace id, grants no capability or authority, and does not expand egress.
- Receive requires an exact configured kind/conversation/thread route or exact bounded dynamic D-to-U/W link plus live-human verification. Strict admits allowlisted humans; lax widens only static prompt ingress. Linking/control remain allowlist-only.
- Receive grants only Tau-issued source replies backed by extension-local state from a canonically confirmed incoming report. Message references use the opaque inert `slack-message:<digest>` form; they are selectors, not bearer capabilities, and native IDs/roots are never accepted as separate model-selected route arguments. Proactive send is a separate current-alias grant and grants no receive, linking, control, edit, inbound-human-reaction routing, or source-reply authority.
- `slack_conversations` is a separate role-authorized inventory surface. It reveals all static aliases plus operator descriptions and configured kind/scope/receive/proactive policy, including receive-only routes; it never reveals native ids/roots, dynamic links, identities, runtime state, or Slack metadata. Use exact tool policy or separate prefixed instances to isolate inventories.
- Fixed threads use their configured root for creates, local replies, edits, reactions, and sends. Parent receive covers children; overlapping parent/child receive is rejected.
- Current session/tool/agent lifecycle and extension route checks are required. Configuration freezes before an authorized post or reaction API attempt, or after successful worker preflight.
- Runtime caches, routes, ownership, selections, and links are bounded. Recent nonempty, control-free Slack occurrence ids of at most 256 bytes are kept in a 4,096-entry process-local FIFO set. A duplicate replays its retained report while canonical confirmation is pending and is dropped afterward. Message and edit occurrences use native ids or stable Slack message coordinates; reactions are cached when Slack supplies an event id. Eviction or restart may duplicate delivery. Edits require same-process canonically confirmed create-report ownership and inbound human reactions require same-process post ownership.
- Occurrence recording still precedes identity lookup, local effects, and report construction. A failure before pending report installation suppresses retry until cache eviction or restart; this is distinct from exact report replay while pending and suppression after canonical confirmation.
- `slack_send` owns a 1,024-entry non-evicting session/process ledger. It
  reserves before I/O and admits at most 64 active delivery workers. Initial
  HTTP and the sole event-driven retry remain off the protocol reader; each
  attempt has a 30-second HTTP timeout, responses are capped at 64 KiB, and the
  retry must begin within the 60-second logical-call horizon. Exact
  lifecycle/config/route authority is revalidated before each attempt and
   report submission; disconnect/EOF retires authority before workers are woken.
  A successful remote post writes transient `message.sent_reported` and then
  transient `tool.result_reported` observations through one serialized
  write-and-flush gate; the harness later derives canonical facts. Only the
  configured publisher's matching canonical `message.sent` downpath echo installs
  posted-message/reaction authority and completes the pending ledger. Any confirmed
  writer failure latches output failure, retires the entire Slack session and all
  receive/send/reaction authority, wakes workers, and terminates the protocol
  connection so harness disconnect cleanup settles retained calls. Replay
  coalesces per call id while canonical confirmation is pending and returns the
  retained stable result after confirmation without reposting. Awaiting-submission,
  pending-canonical, completed, definitive, cumulative
  ambiguity/copy range, and cancellation states are retained. Full capacity
  rejects before freeze/I/O. One
  initial-plus-one byte-identical retry is deliberately at-least-once: an
  ambiguous first attempt can leave one or two Slack copies; two ambiguous
  attempts can leave zero, one, or two. Restart clears this
  boundary; there is no durable outbox, `client_msg_id`, reconciliation, or
  exactly-once guarantee. Already-started synchronous HTTP is process-owned and
  may outlive the protocol `run` return for at most its 30-second request timeout;
  retired workers cannot retry or restore local authority.
- Slack HTTP/identity/post diagnostics are closed typed categories. Raw provider
  bodies/codes/headers/errors, tokens, native ids, mention text, and message text
  do not enter displays, model errors, notices, or logs. Agent mrkdwn rejects raw
  native controls; bridge-owned reflected output is escaped, disables mrkdwn and
  link expansion, and is component/final bounded.
- Agent-authored text cannot contain raw `<@`, `<!`, or `<#` Slack controls.
  `mention_source_user` defaults false and can generate only the exact verified
  human bound to a live `reply_to`; it is invalid for configured destinations
  and for the bot/Slackbot. The generated mention is frozen with the exact
  byte-identical retry body. `ToolStarted` is the scoped-tool lease for one
  logical call, and Slack revalidates current authority before report submission.
- Identity/API outages fail closed. Slack, workspace administrators, members, and Slack Connect participants may read transported text; this is not end-to-end encrypted.
- Supported ingress uses one persistent serial in-memory FIFO bounded at 64 queued/in-flight occurrences. Capacity is reserved before ACK; terminal non-report work releases it, while submitted reports retain it until exact canonical confirmation. Missing echoes therefore saturate admission, stop later ACKs, and let Slack retry after reconnect. Socket Mode ACK remains separate from Tau commit. Actor failure and harness-writer closure stop later ACK admission. Reconnect preserves admitted order; session/config/process teardown invalidates late authority. Process death after ACK can still lose work.
- Socket Mode sends its first client Ping after 10 seconds and repeats every 10 seconds. An independent deadline reconnects exactly 40 seconds after the latest Pong. No other traffic refreshes liveness, and Ping/Pong/ACK writes race both shutdown and that deadline so write backpressure cannot defeat either bound. Reconnect still revalidates the exact installation pair.
- Socket Mode caps every Tungstenite frame and complete text or binary message at 256 KiB of payload before decode; equality is accepted. The first excess byte returns the content-free `Slack websocket frame failed` category, drops the socket, and follows the existing one-to-30-second reconnect backoff. The post-decode text check remains defense in depth. Loopback coverage exercises configuration plus unfragmented, raw-fragmented, and binary exact/excess boundaries. Recheck this invariant when changing the limit, Tungstenite configuration, frame handling, or reconnect classification.
- Latency observability is TRACE-only and non-durable. It permits process-local volume/order correlation but never native IDs, payloads, tokens, URLs, response bodies, agent IDs, or stable hashes. Keep retention bounded and never promote occurrence/request ordinals to metric labels.
- Recheck these invariants and the adversarial matrix in `specs/ARCH-tau-ext-slack.md` when changing config freeze, routing, sender admission, mutations, dedup, lifecycle, or message-report submission.

## Agent-invoked reaction boundary

`slack_react` is a separately role-gated, disabled-by-default externally visible
action (`slack:react`). It accepts only exact locally retained Tau-issued refs,
never native Slack IDs or aliases, and revalidates live source/config authority.
Adds establish bounded same-agent runtime ownership only after unambiguous
success and confirmed local result write/flush; removes require that ownership
and clear it only after the same boundary. Failed output retires the whole Slack
session and retains no reaction authority. It never retries or compensates a
possibly completed Slack effect, so lifecycle clearing may leave remote residue
rather than risk cross-agent or cross-session removal. Local flush is not a
harness commit acknowledgement. The optional surface requires only
`reactions:write` and does not add listing/discovery. Slack members,
administrators, Slack Connect, and workflows can observe or act on bot
reactions.
