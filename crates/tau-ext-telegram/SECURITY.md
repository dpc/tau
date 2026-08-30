# tau-ext-telegram security and reliability notes

Mandatory ingress/sent reports and sole tool terminals use checked output.
Failure stops the current provider batch and retires the extension connection;
optional progress and notices remain detached and best effort.
Checked configuration errors use the same sticky failure signal. A shared
publication/shutdown gate ensures forced output teardown joins the poller,
while ordinary disconnect does not wait for a provider long poll.

Desired listener registrations are the only restart-durable sidecar routing
state. They use the configured instance's harness-owned Session-scope extension
data and contain only a strict version plus Tau agent IDs. Missing state is
empty; malformed, unsupported, or unreadable state fails configuration. Replay
performs no Telegram I/O: current loaded membership gates restoration at
`session.replay_complete`, and stale desire is removed before live authority is
reactivated. Bot tokens, gateway credentials, Telegram identities, native
routes, message text, selections, and checkpoints do not enter this file.
Replacement errors are read back through the same exact-session RPC. An
unchanged snapshot is a known failure. A visible target snapshot whose
parent-directory durability sync failed remains indeterminate across a crash;
read-back failure is likewise indeterminate. The extension retires without a
tool terminal rather than continuing to route from either uncertain outcome.

`std-telegram` is a disabled-by-default personal text bridge. The configured
extension process and optional same-UID gateway are cooperative local
components, not hostile-code sandboxes. The gateway socket uses mandatory
mutual authentication, so merely opening it grants only minimal readiness and
challenge access, not route, delivery, ACK, or send authority. This does not
protect keys held in process memory from malicious same-UID `/proc`, ptrace, or
memory access. Telegram, Bot API endpoints, sender
metadata, commands, and message text remain untrusted external input. The
allowlist authenticates one Telegram numeric sender for admission; it does not
make message text trustworthy or grant Tau tool authority.

- Bot tokens are secrets. Diagnostics and update-stream lock metadata may expose
  only the API base and domain-separated stream fingerprint, never the token or
  token-bearing URL. Production Bot API endpoints require HTTPS; plaintext is
  loopback-test-only.
- Gateway credentials use exactly 32 bytes encoded as 64 lowercase hexadecimal
  characters; surrounding whitespace is rejected. The transcript binds the
  numeric protocol version, key selector, gateway and client generations,
  extension identity, both fresh nonces, and a distinct client/server proof role.
  Key selectors are domain-separated keyed-BLAKE3 digests truncated to 16 bytes.
  Client and server proofs are keyed-BLAKE3 over u32be-length-prefixed transcript
  fields and are compared in constant time. Every malformed or unauthorized
  pre-authentication frame receives the same bounded `authentication failed`
  response followed by connection close.
  Current and previous key slots must differ, configuration diagnostics redact
  their contents, and the sidecar accepts only canonical generation, nonce, and
  proof encodings.
- One monotonic deadline covers each complete authentication exchange, so
  byte-by-byte progress cannot retain a handler indefinitely. Authority is
  installed only after the authenticated response is written. Reconnect fencing
  and exact reannouncement follow the governing gateway specification.
- Every successful Bot API response body has a Tau-owned inclusive 10 MiB cap
  after HTTP framing. Exactly 10 MiB reaches JSON decoding; 10 MiB plus one
  fails as a `Protocol` error before decoding. The cap applies equally to
  Content-Length and chunked responses, rather than trusting a declared length.
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
- Desired registrations are durable as described above; their currently active
  local routes, links, selections, offsets, and checkpoints are process-local.
  Process death forgets pending reports; a fresh process drains
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
  authenticated local protocol does not treat lease state as durable ACK
  authorization.
- One cancellable sidecar supervisor owns gateway connect/reconnect. While the
  socket is absent or failed, socket delivery, ACK transmission, and outbound
  send fail closed; no
  token lookup, stream lock, Telegram HTTP fallback, or local polling occurs.
  An exact canonical echo validated while disconnected remains a local pending
  ACK obligation and is transmitted only through a subsequently validated live
  connection.
  Every replacement connection validates a fresh `hello` generation, reannounces
  an exact current route snapshot, and commits it with
  `complete_reannouncement` before becoming live. Bounded
  backoff limits retry load, while configuration generations plus joined
  cancellation prevent stale workers and responses from restoring authority.
  Pending registration responses expose no durable deliveries, and pending
  heartbeat, unregister, send, and ACK operations fail before authority-bearing
  side effects.
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
- Authentication primitive tests protect strict key and field parsing, key-slot
  rotation, transcript role separation, proof comparison, uniform malformed-field
  rejection, absolute deadlines, final-response-loss cleanup, fencing, and exact
  replacement reannouncement. Re-run and revisit those tests and this threat
  boundary whenever changing key parsing or slots, transcript framing, proof
  derivation or comparison, pre-authentication errors, deadlines, fencing, or
  reannouncement.
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
