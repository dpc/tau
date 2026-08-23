# SPEC-tau-cli-notice-filtering: Notice filtering

## Record justification

Notice purpose and filtering span protocol payloads, harness producers and
routing, typed CLI event projections, retained transcript state, and persisted UI
settings, so no one local artifact can own the complete contract.

Harness/UI notices carry a presentation purpose orthogonal to severity and
provenance. `response` is exact feedback for an explicit action by the receiving
UI, `alert` is an unsolicited condition the user must see or may need to act on,
and `diagnostic` is ambient lifecycle, developer detail, or a human projection of
model control. UI policy must use this purpose rather than parsing notice text or
inferring meaning from its carrier or `kind`.

Compact transcript mode shows responses and alerts and hides diagnostics.
Verbose mode always shows responses and alerts, while `:set notice-level
<level>` and persisted `cli.json` `notice_level` filter diagnostics. Critical
notices remain defensively visible in either mode regardless of purpose.
Changing modes or diagnostic settings reprojects retained blocks at their
original transcript positions.

Model-facing status reminders, context-size advisories, timer wakeups, and other
internal-prompt projections are diagnostics. Compact mode dominates
`show-internal-prompts`, so enabling that subfilter cannot reveal them until the
UI returns to verbose mode. These presentation projections do not affect prompt
facts, model context, harness emission, protocol delivery, or journals.

Requester-directed responses are live-only and delivered only to the initiating
UI. Configured extensions cannot assign notice purpose: routine extension notice
requests always become diagnostics, while the harness owns alert paths such as
extension configuration errors.

Successful manual-compaction acceptance and start are routine `info` lifecycle
notices and use ordinary status styling. Pre-start and transaction failures
remain terminal tool errors and retain error presentation.
