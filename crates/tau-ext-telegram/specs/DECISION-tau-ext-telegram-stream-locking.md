# DECISION-tau-ext-telegram-stream-locking: Telegram update streams are Tau-locked per state root

Authority: unconfirmed

Telegram's Bot API `getUpdates` cursor is singleton state for one API base plus
bot token. Before this extension polls or drains that stream, Tau takes an
advisory exclusive OS lock scoped to the stream identity so another Tau process
sharing the same Tau state root fails closed instead of racing update offsets.

The lock key uses a non-secret BLAKE3 fingerprint over API base plus bot token.
Lock metadata may include owner process details, API base, and that fingerprint,
but never the raw bot token. The lock is advisory and local to processes that use
the same Tau state/ext root; separate users, containers, or explicitly separate
Tau state roots are outside this coordination scope.

Telegram webhooks are mutually exclusive with `getUpdates`. Starting active
polling checks `getWebhookInfo` after acquiring the local lock and fails visibly
if a webhook is configured, without deleting the webhook or dropping pending
updates. Subsequent registrations join the already-owned stream. Because
Telegram does not expose active long-poll ownership, HTTP 409 `getUpdates`
conflicts are treated as reactive contention diagnostics: the extension surfaces
a warning and clears active registrations so agents do not believe they still own
the update stream.
