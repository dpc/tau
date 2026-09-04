# tau-ext-xmpp security and reliability notes

`std-xmpp` is disabled by default. XMPP credentials are secrets and must never
enter logs, diagnostics, structured lifecycle events, or fixtures. Production
traffic uses certificate-validated TLS, but there is no OMEMO or other end-to-end
encryption: servers, operators, recipients, and MUC occupants may read accepted
message parts.

Configured allowlists, fixed destinations, registration-first sending, direct
full-JID matching, and MUC real-JID or explicit trusted-membership checks are the
authorization boundary. Incoming text remains untrusted external input. A
worker-side conversation left after failed cleanup must not retain local routing
authority.

MUC real-JID presence state is scoped to active rooms and the exact pending room
currently joining. The worker retains at most 256 occupant mappings per room and
1,024 total. Crossing either inclusive limit clears and quarantines only the
affected room; every groupchat from that room then fails closed before
trusted-membership fallback until a fresh join rebuilds the roster. An oversized
initial roster fails registration.

Current tool delivery still runs readiness, registration, and sequential multipart
send waits on the serialized protocol reader. Unregister and lifecycle retirement
instead revoke the exact process-local registration lease first, enqueue bounded
best-effort remote cleanup without waiting, and revalidate the lease immediately
before inbound publication. Its checked
mandatory publication boundary is current under the approved audit fix. The
remaining executor, FIFO, deadline, generalized generation, and observability
design in
[SPEC-tau-ext-xmpp-tool-delivery-lifecycle](specs/SPEC-tau-ext-xmpp-tool-delivery-lifecycle.md)
is prospective and not implemented or authorized for implementation.

That end state bounds one process-local FIFO at 32 live logical bodies, about
4 MiB at the 128 KiB message limit, plus metadata and one rendered stanza.
Register and send reservations have one absolute 60-second deadline including
queue time. These limits bound memory and head-of-line denial of service but do
not bound `tau-client`'s separate output queue.

Registration generations now revoke local authority synchronously before remote
cleanup for explicit unregister, unload, session shutdown, disconnect,
and output-loss retirement. Cleanup is best effort and may fail without
restoring routes. Configuration and generalized output/executor generations
remain prospective. Frozen plaintext bodies remain
in memory until terminal disposition and are then dropped promptly. No
persistent outbox or allocator zeroization is claimed.

Multipart delivery remains non-transactional. Accepted earlier parts can remain
visible. Once a current stanza is handed to the transport, cancellation, timeout,
or worker failure can leave zero or one copy of it; automatic retry is forbidden.
Successful report-before-result output remains non-transactional and does not
acknowledge harness commit or remote receipt.

Prospective observability uses closed, content-free outcomes. It may include
process-local ordinals, operation kind, queue depth, generations, durations,
part counts, revocation cause, and bounded remote-copy classification. It must
never include bodies, complete arguments, passwords, tokens, JIDs, room or nick
names, or raw server errors. Exact prospective behavior is
[SPEC-tau-ext-xmpp-tool-delivery-lifecycle](specs/SPEC-tau-ext-xmpp-tool-delivery-lifecycle.md).
