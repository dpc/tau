# DESIGN-tau-cli-watch-lifecycle-rendering: Watched-agent lifecycle event rendering

Constrained by [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md).

Status: confirmed, 2026-07-10, dpc

Receiver-side watched-turn state records are harness-authored lifecycle events,
not messages authored by the watched agent. The CLI derives their presentation
from the structured initial/state payload and watched identity and renders a
compact single-line status such as `Watching <agent> · idle` or
`<agent> · turn started`, without displaying the compatibility message body.
These lifecycle statuses bypass `show-messages` because they are state events,
not agent-to-agent message content.

Genuine watched response and direct-user-prompt notifications retain their
`WatchResponse` and `WatchPrompt` kinds, normal sender/watcher attribution, and
existing summary/full/hidden `show-messages` behavior. Lifecycle presentation
must not reclassify or hide that content-bearing notification history.
