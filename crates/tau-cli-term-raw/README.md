# tau-cli-term-raw

Terminal prompt with async output support for tau.

## Rendering strategy

We render directly to the **normal terminal buffer** (no alternate screen).
This lets us use the terminal's native scrollback to preserve earlier output
that has scrolled off the top of the viewport. Three rendering paths handle
different situations, chosen by the redraw loop in `redraw_loop()`.

### Path 1 — Differential update (common case)

On each redraw the layout engine produces a flat list of `all_lines` covering
all content: history, live blocks, the prompt, and status zones. We slice the
last `height` lines as the visible viewport and pass them to
`Screen::update()`, which diffs against what was previously on the terminal and
emits only the escape sequences needed to update changed cells. This minimizes
terminal I/O — important over slow SSH connections.

Cursor movement is always relative (`MoveUp`, `\r`, `\n`, `MoveToColumn`) —
never absolute positioning. Downward movement uses `\n` rather than `MoveDown`
because `\n` scrolls the terminal when the cursor is at the bottom edge, while
`MoveDown` silently stops.

This diff approach is borrowed from fish shell's `screen.rs`.

### Path 2 — Scrolling render (overflow into scrollback)

When new content causes `visible_start` to increase (lines pushed off the top
of the viewport), a plain differential update would lose those lines — they
were rendered to the terminal previously but `Screen::update()` only knows
about the visible slice, so it can't push them into scrollback.

Instead, `Screen::render_scrolling()` diffs against the *full* content array
(not just the visible slice). It finds the first changed line and renders from
there downward using `\r\n` between lines. When `\r\n` is emitted while the
cursor is at the bottom terminal row, the terminal's native scroll mechanism
kicks in — the top screen row is pushed into the **scrollback buffer** and
everything shifts up. Because changed lines are rendered in top-to-bottom
order (overwriting their screen rows before they scroll off), the correct
content enters scrollback.

The key insight: scrollback is populated as a *side effect* of rendering, not
as a separate step. Content must be written to the terminal before it can
scroll into the scrollback buffer. This is why a simple "emit `\n` to scroll"
approach does not work — it would only push whatever happened to be on the
screen previously, not the new content.

This technique is borrowed from the
[Pi coding agent](https://github.com/ArtificialWisdomAI/pi-monorepo)'s TUI
renderer (`@mariozechner/pi-tui`), which renders lines sequentially and lets
`\r\n` at the viewport bottom push content into scrollback rather than
managing scrollback internally.

### Path 3 — Full render (resize)

On terminal resize or invalidation, `full_render()` clears the screen **and
scrollback** (`\x1b[2J\x1b[H\x1b[3J`), then outputs the configured suffix of
no-rubber scrollable log/history lines plus the fixed tail. Lines that overflow
the viewport scroll into native scrollback naturally, rebuilding recent history
with the current width while deliberately omitting older history. Temporary
rubber is dropped, so if the replayed transcript fits, the prompt sits directly
under content instead of being bottom-pinned by blank rows.

Scrollback is cleared on resize because the old scrollback contains lines
wrapped at the old terminal width. Replaying logical content produces correctly
reflowed scrollback for the new width.

## When mutations need a full redraw

The diff renderer (Path 1) only repaints the **visible viewport** — the last
`height` rows of `all_lines`. Anything above that lives in the terminal's
scrollback buffer and is unreachable by cursor positioning or differential
updates. Mutating a block whose rendered rows have scrolled into scrollback
without forcing a `full_render` leaves a *fossilized* copy in scrollback that
no longer matches the program's state.

The layout from top to bottom:

```
history          ← oldest, scrolls into scrollback first
above_active     ← live blocks (streaming responses, in-flight thinking)
above_sticky     ← pinned blocks (model status chip, queued user prompt)
input area       ← capped prompt viewport + optional hidden-row indicator
suggestions      ← completion menu
below            ← anything below suggestions
```

Everything from `above_active` down is **bottom-anchored** — it sits at the
tail of `all_lines`, near the input cursor. As long as the bottom-anchored
zones fit in `height` rows, they are entirely inside the visible viewport and
the diff renderer can update them in place.

`above_active` is still a generic ordered live-block zone, but callers may keep
their own semantic sub-order inside it. The chat UI uses:

```
thinking → compaction → streaming response → active tool summary/tool calls
```

Use `TermHandle::push_above_active_before_any` when inserting a live block that
must appear before existing active anchors without rebuilding the whole output
snapshot. The helper removes any existing reference to the moved block, inserts
it before the first matching active anchor, and appends it when no anchors are
currently active. This is safe for live-tail/active-area ordering that remains
inside the bottom-anchored viewport; it is not a general mechanism for rewriting
history or already-scrolled scrollback rows.

### Safe mutations (just call `TermHandle::redraw()`)

- Editing the input buffer or prompt.
- Updating the model status chip, suggestions, anything in `below`.
- `set_block` on a streaming live block in `above_active` — the most
  common case (response text appending, in-flight thinking growing,
  tool-progress updates).
- `print_output` of a brand-new block. The new block lands at the
  bottom of `all_lines`, so by definition it appears in the visible
  window when first emitted; its arrival may push earlier content into
  scrollback (Path 2), which is the natural way scrollback gets
  populated.

### Mutations that require `TermHandle::invalidate_screen()` (Path 3)

`invalidate_screen()` sets a flag that forces the next redraw through
`full_render` — clear screen + clear scrollback (`\x1b[2J\x1b[H\x1b[3J`),
then re-emit the configured suffix of no-rubber scrollable history plus the
fixed tail. Use it for:

- **`set_block` on a block that may have scrolled out of the viewport.**
  Anything in `history`, including the most recent finalized block once
  later content has arrived. Examples: toggling diff expand/compact via
  `/show-diff`, hiding/showing thinking via `/show-thinking`.
- **Reordering the zone lists in a way that affects past rows.** A block
  that's still in `history` but has scrolled off can't be moved by
  `set_block` alone — diff render won't reach it.
- **Any geometry change.** Resize and `resume_after_external` already do
  this internally.

### The one edge case: live blocks larger than `height`

A live block in `above_active` can in theory grow taller than the visible
viewport. When that happens, its top rows have been written to the terminal
and scrolled into scrollback while its tail is still being updated. In-place
`set_block` updates only repaint the visible tail of the block; the
scrollback fossil is now stale.

This is currently invisible because streaming is **append-only**: text only
grows, characters are never retracted mid-stream, and `set_block` calls only
extend the tail. If we ever wanted retractable streaming or out-of-order
edits within a long live block, those updates would also need
`invalidate_screen()`.

The provider protocol mirrors this assumption for visible assistant/reasoning
progress: intermediate `provider.response_updated` events carry only appended
text deltas, while `provider.response_finished` carries the complete final
response.

## Known limitations

- **Resize clears pre-tau scrollback history.** Any terminal output from
  *before* tau started (shell commands, etc.) is lost on the first resize.
  Tau's configured replay window is rebuilt by the full re-render, but older
  clipped Tau rows remain only in Tau's logical history, and the pre-tau
  scrollback cannot be recovered. This is an inherent trade-off of rendering to
  the normal terminal buffer without an alternate screen.

- **Content never displayed on screen cannot enter scrollback.** Path 2
  handles the common case where previously-visible lines scroll off. However,
  if a single update adds more new lines than the terminal height (e.g. a
  very long agent response arriving all at once), lines that were never
  on screen will not appear in scrollback. In practice this is rare because
  streaming responses grow incrementally.

## Layout zones

All content blocks are stored in a central map keyed by `BlockId`. Separate
ordered lists reference them for rendering (top to bottom):

1. **History** — persistent output (append-only).
2. **Above active** — mutable blocks (e.g. streaming responses).
3. **Above sticky** — blocks pinned right above the prompt.
4. **Input area** — a prompt-local viewport over left-prompt + user input +
   right-prompt, capped to one third of terminal height. It may include a
   compact hidden-row indicator; this local viewport is separate from terminal
   scrollback and history viewporting.
5. **Suggestions** — completion menus below the prompt.
6. **Below** — status bars and other persistent bottom content.

## Threading model

Several execution contexts cooperate:

- **Downstream event loop** — the caller's thread. Calls
  `Term::get_next_event()`, receives raw input through an internal channel,
  handles prompt editing internally, and surfaces high-level events. Shutdown and
  virtual input close are sticky EOF states: once observed, later input reads
  return `Event::Eof` without waiting for another terminal event.
- **One-shot real-input helper** — for real terminals, each blocking
  `crossterm::event::read()` runs in a helper thread and sends one raw event or
  read error back to the downstream event loop. Shutdown wakes the downstream
  event loop through the same internal channel; because crossterm reads are not
  portably cancellable, at most one detached helper may remain blocked until
  stdin produces an event or the process exits. Any helper result that arrives
  after shutdown is ignored, or dropped if the terminal has already gone away.
  Helpers are not persistent, so normal external programs such as `$EDITOR` are
  launched only after the current input read has completed and do not race Tau
  for stdin.
- **Virtual input bridge** — tests keep the public `Sender<RawEvent>` returned
  by `Term::new_virtual()`. A small bridge thread forwards those events into the
  internal input channel and sends the sticky EOF wakeup when all virtual input
  senders are dropped.
- **Redraw thread** — blocks on a coalescing notify channel, wakes up, reads
  shared state under a mutex, and renders via one of the three paths above.

Any thread holding a `TermHandle` can mutate zones and trigger a redraw.
Multiple redraws coalesce into one via the notify channel.

Callers that temporarily install an off-screen output snapshot and then restore
the visible snapshot must wrap the whole sequence in
`TermHandle::with_output_transaction`. Ordinary `TermHandle` output mutations
from cloned handles wait for that transaction, so local terminal output cannot
attach to the temporary snapshot.

## Test strategy

Most tests use `Term::new_virtual()` so the input loop receives injected
`RawEvent`s and the redraw thread writes to an in-memory `Write`. Rendered bytes
are fed into `vt100::Parser`, which lets tests assert visible rows, scrollback,
cursor placement, and terminal side effects without owning the real terminal.

The suite covers the renderer at several levels: low-level full-redraw and
scrolling helpers, model-vs-vt100 scrollback equivalence checks, redraw
coalescing and `redraw_sync`, resize/full-redraw rebuilds, prompt history,
completion, paste/newline normalization, and local prompt scrolling. Tests that
exercise terminal ownership use the virtual pause/resume hooks to verify that
the redraw thread stays silent while an external editor or picker owns the
terminal.

## References

- Fish shell screen rendering: <https://github.com/fish-shell/fish-shell/blob/master/src/screen.rs>
- Pi coding agent TUI: <https://github.com/ArtificialWisdomAI/pi-monorepo>
  (specifically `@mariozechner/pi-tui`, `src/tui.ts`, the `doRender()` method)
