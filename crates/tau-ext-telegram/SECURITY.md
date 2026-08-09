# tau-ext-telegram security and reliability notes

`std-telegram` is a disabled-by-default personal text bridge. The configured
extension process and optional same-UID gateway socket are cooperative local
components, not hostile-code sandboxes. Telegram, Bot API endpoints, sender
metadata, commands, and message text remain untrusted external input. The
allowlist authenticates one Telegram numeric sender for admission; it does not
make message text trustworthy or grant Tau tool authority.

- Bot tokens are secrets. Diagnostics and update-stream lock metadata may expose
  only the API base and domain-separated stream fingerprint, never the token or
  token-bearing URL. Production Bot API endpoints require HTTPS; plaintext is
  loopback-test-only.
- Local polling takes one advisory lock per API-base-plus-token stream and
  refuses active webhooks or observed HTTP 409 contention. The lock coordinates
  only Tau processes sharing one state root; out-of-band consumers remain
  possible.
- Chat/user allowlisting, active-chat policy, command parsing, and target
  selection run before routed report construction. Numeric Telegram identities
  become opaque sender/message references. The model cannot select a Telegram
  destination.
- A routed local-poll update enters an ordered process-memory checkpoint before
  its report becomes visible on the extension output. Only this configured
  publisher's live canonical `message.delivered` event with the exact target
  agent, message identity, and opaque private report ID acknowledges it.
  Transient reports, replayed facts, wrong publishers, and partial collisions do
  not advance the cursor.
- Non-routed updates emit no Tau event and acknowledge when processing returns.
  Telegram replies are best effort. If an earlier routed checkpoint blocks the
  cursor, Telegram may redeliver later non-routed commands and duplicate replies.
  `/start` and `/select` replace the same local state idempotently; no remote
  effect is exactly once.
- The cursor advances only through the contiguous acknowledged prefix across
  routed and non-routed checkpoints. Missing routed echoes replay the exact
  retained report without recomputing mutable target selection. Retry delay
  grows from 250 milliseconds to a five-second cap; unrelated canonical traffic
  does not wake that delay. This bounds retry rate, not Bot API response memory.
- Registration loss stops new polls and releases stream ownership after
  in-flight requests return. Re-registration re-enters backlog drain: retained
  routed checkpoints replay, while unseen stale updates are discarded until an
  empty batch. Stream-identity changes and configuration removal clear local
  checkpoints. Same-stream configuration generations and polling-contention
  shutdown preserve already submitted checkpoints, while stale in-flight
  responses cannot process updates.
- Local registrations, links, selections, offsets, and checkpoints are not
  durable. Process death forgets pending reports; a fresh process drains
  Telegram backlog and can therefore discard a routed update whose canonical
  echo was lost before the crash. Conversely, report replay before a crash may
  produce duplicate canonical facts, wakes, or model work. There is no durable
  outbox, cross-process deduplication, or exactly-once recovery.
- Gateway mode moves token, polling, durable cursor/checkpoint state, allowlist,
  destination, and required Telegram reply authority into the standalone
  operator-supervised daemon. It persists routed reports before bounded,
  non-destructive socket exposure. Only an exact live canonical echo from the
  configured sidecar publisher causes `ack_delivery`; lease loss and restart
  replay the retained route rather than deleting or recomputing it. The ACK
  includes the persisted report ID and frozen route, so the gateway accepts only
  that exact durable pair even after the live lease disappears; this same-UID
  local protocol does not treat lease state as an authentication boundary.
- One cancellable sidecar supervisor owns gateway connect/reconnect. While the
  socket is absent or failed, socket delivery, ACK transmission, and outbound
  send fail closed; no
  token lookup, stream lock, Telegram HTTP fallback, or local polling occurs.
  An exact canonical echo validated while disconnected remains a local pending
  ACK obligation and is transmitted only through a subsequently validated live
  connection.
  Every replacement connection validates a fresh `hello` generation and
  reannounces an exact current route snapshot before becoming live. Bounded
  backoff limits retry load, while configuration generations plus joined
  cancellation prevent stale workers and responses from restoring authority.
  Non-routed updates commit only after required replies and mutations succeed.
  The same state-file transaction commits an ACK, advances its contiguous
  prefix, and retains one of 128 content-free report-ID/route retry
  authorizations. Replies and reports may duplicate after crashes or lost
  acknowledgements; no remote effect is exactly once. State-save failures before
  rename roll back. A
  parent-directory sync failure after rename retains the installed candidate,
  poisons the shared state owner, refuses further state operations, and requires
  gateway restart rather than claiming rollback. Review
  [SPEC-tau-telegram-gateway](specs/SPEC-tau-telegram-gateway.md) separately when
  changing it.
- Hermetic tests force early canonical echoes, missing and unrelated echoes,
  mixed/out-of-order checkpoints, retry bounds, listener reconnect drain,
  process restart drain, stream changes, and maximum update-ID arithmetic.
- Gateway exit statuses are non-secret supervisor control data. Typed failure
  mapping never encodes tokens or message content in the status, and bounded
  stderr diagnostics redact the configured token even if a remote response
  reflects it.

Recheck these safeguards when changing Bot API decoding or bounds, update
ordering, retry timing, output submission, canonical message correlation,
configuration generation, registration/shutdown behavior, backlog drain,
gateway delivery, or any persisted Telegram state.
