# SPEC-bridge-receiver-designation: Sticky external bridge receivers

## Status

The shared protocol, harness lifecycle support and SDK helpers implement this
contract. Discord, GitHub and Zulip integrate the bridge-owned parts separately;
other bridges retain their behavior and XMPP is excluded.

## Record justification

Receiver selection and lifetime span harness membership and immutable creation
facts, while designation persistence, registration and input ownership belong to
independently configured bridge processes, so no single local artifact owns the
handoff and restart contract.

## Authority and selection

Only configured `MessageBridge` peers may resolve a current-session receiver.
Resolution is private and never supplies a roster or assigns external input.
Exact register validation derives the caller from a live tool routed to that
connection; it never accepts an arbitrary caller identity or substitutes another
agent. Existing tool policy remains authoritative.

An eligible saved designation wins. Otherwise automatic selection chooses the
oldest eligible loaded agent by immutable creation timestamp, then agent ID;
unknown legacy timestamps sort last. Eligibility requires live ordinary receiving
lifecycle, committed creation and current-session membership, no pending unload,
and matching immutable creation role when constrained. One-shot extension queries
are not receiving endpoints. Busy or initializing receivers are not rebalanced.
Unknown configured roles fail visibly. Unloaded IDs are never force-loaded.
Restoration/readiness and already-starting eligible agents precede absence-based
creation decisions.

`register_on_start` defaults false: restore a saved manual designation but never
first-select or create automatically. True selects automatically; only a request
following actual admitted external input may create, and only for an explicit
role in ordinary target mode. Missing role selects existing any-role agents and never guesses a creation
role. Creation uses ordinary role preparation without a bootstrap prompt and
records a reserved noninheritable bridge receiving purpose in `agent.started`.
It retains ordinary tool-capable lifecycle after replies and on cold restore,
without pretending to be a cooperative peer endpoint. Peer pool fairness remains
unchanged.

An opt-in `canonical_main` target replaces ordinary selection with the committed
current-session main identity. Register and Restore reject any other identity,
including saved same-role designations. Select and Ensure may choose main through
the bridge's normal successful designation update; they never rewrite a saved
designation on failure. An optional immutable creation-role constraint
applies. Only Ensure after admitted input may call the existing prompt-free main
construction operation. A pending candidate has no authority until binding
commits. If bound main is missing or unavailable, the harness must not fall back
to an ordinary agent or rebind main. This target adds no durable identity fact.
Before sending this target, SDK callers must require the harness's advertised
protocol major to match the SDK's compiled major, currently 19. All minors within
that major pass. Missing revisions and other majors, including future majors,
fail closed to avoid sending a constraint the harness might ignore. Protocol 16.5
introduced the target; that historical feature floor does not permit cross-major
use after coordinated protocol migrations.

## Bridge-owned designation

Each instance serializes designation changes. A supported versioned concrete-ID
snapshot in Session ExtensionData is authoritative after successful replacement;
registration succeeds only after its snapshot succeeds. An ordinary failed write
preserves the prior designation; uncertain outcomes require read-back or failure
without guessed authority. Late automatic results cannot overwrite newer manual
handoffs. Same-agent registration is inert. Failure to resolve does not erase a
saved ID. Legacy accepted registration supplies only one-time unambiguous migration
evidence, never authority over an existing snapshot.

The current `harness.session_dir` projection supplies session durability through
its status, including to late historical subscribers; paths are not authority.
Ephemeral sessions keep designation in memory, not persistent extension storage.
Session shutdown cannot provision receivers.

Already assigned messages, canonical facts and ACK correlations remain with their
original concrete targets. Source/reply/attachment references never transfer.
Receive designation does not expand outbound grants. Old unregister/disable forms
are rejected rather than creating a pause mechanism.

After an actual successful handoff, send a best-effort fixed-text notice using the
existing loaded-only internal prompt request for the previous receiver. Do not
replay notices, load old targets, fabricate provider messages, or notify for initial
selection and same-agent no-ops. Existing arrival scheduling, external provenance
and asynchronous ACK/disk tradeoffs remain unchanged.

The user approved this semantic package on September 23, 2026 under
[GATE-persistence-and-extension-interface-change-approval](GATE-persistence-and-extension-interface-change-approval.md).
