# DESIGN-tau-cli-prompt-editor-context: Per-agent prompt-editor response context

Status: confirmed, 2026-06-25, dpc

The terminal UI keeps visible transcript state in renderer fields and snapshots
hidden agent transcripts in `AgentUiState`. Response text used by the external
prompt editor's trailer follows the same per-agent snapshot boundary: current and
last assistant response context belongs to the viewed/no-agent transcript, while
prompt-local fields such as previous prompt and trailer recovery stay with the
active input/editor flow.

Live UI blocks that have a distinct start/completion lifecycle must complete in
the same transcript snapshot that rendered their start block, even if the user
switches viewed agents before completion arrives. Hidden completion folding may
temporarily restore the owning agent or no-agent snapshot, update/remove the live
block there, then restore the actually visible transcript without publishing
hidden prompt-editor context.

When routing an event for a hidden agent, the renderer may temporarily restore
that hidden snapshot into renderer fields to reuse normal folding code. During
that hidden fold it must not publish hidden response context through shared
input-loop mirrors such as `EditorContext`; Ctrl+O and other prompt actions must
continue seeing the actually visible/no-agent context until the user explicitly
switches transcripts.

The hidden restore/fold/save/restore sequence must also be atomic with respect
to terminal output emitted through cloned `TermHandle`s, such as local
client-side notices. Hidden folding installs a temporary output snapshot in the
shared terminal handle; local output must wait until the actually visible
snapshot is restored so it cannot be appended to a hidden agent transcript by a
cross-thread race.

The initial no-agent/start-new-agent screen is not a durable transcript boundary.
Startup or post-`/session new` status, action, and extension output that is
visible there is the beginning of the first selected/created agent conversation.
Selecting that first agent therefore adopts the visible no-agent output in place,
without replacing the terminal snapshot or clearing scrollback. Pending no-agent
action completions and extension lifecycle owners are retargeted to the adopted
agent only in this initial no-swap case so later completions update the same
visible conversation. Explicit `/agent none` and `/agent new` after leaving an
agent are different: they intentionally create a protected no-agent snapshot, and
fresh agents must not inherit output or pending owners from that explicit global
view.
