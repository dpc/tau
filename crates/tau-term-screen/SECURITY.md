# Security and reliability notes

`tau-term-screen` is a synchronous terminal layout and rendering library. It does
not own terminal input, process spawning, networking, persistence, or background
tasks; callers own those boundaries and must bound the amount of text they ask the
crate to lay out or render.

## Terminal output trust boundary

Caller-provided text is untrusted with respect to terminal control sequences.
Styles are represented as structured values, not inline ANSI bytes. Layout and
cell emission sanitize tabs and non-newline control characters before output so
styled text cannot inject arbitrary terminal controls such as clears, cursor
moves, or OSC sequences.

Newline graphemes are layout separators, not inline cells. Hand-built public
`Cell` values are normalized again before diffing/caching and before emission so
callers cannot bypass the styled-text sanitizer by constructing cells directly.

Important safeguard tests live in `src/screen/tests.rs`, especially the layout
control-character sanitization, public-cell sanitization, and normalized-cache
regression tests.

## Screen cache ownership

`Screen` tracks what Tau believes is currently displayed. If anything else writes
to the same terminal area, clears it, switches alternate-screen state, or changes
the terminal width, the caller must invalidate or reset this cache before the next
differential update. Use `Screen::invalidate`, `Screen::erase_all`,
`Screen::reset_to`, and `Screen::set_width` according to the external operation.

Scrolling rendering depends on the caller passing the previous viewport top and a
current terminal height. Resize/full-redraw paths should rebuild the visible model
rather than trusting stale row positions.

Relevant safeguard tests cover pending-wrap movement, scrolling growth, changed
rows moving into scrollback, and bounded differential output.

## Resource bounds

The crate performs in-memory layout over the provided spans and rows. It does not
apply policy limits to message size, block count, scrollback size, or render
frequency. Callers that render untrusted or very large content must impose those
bounds before constructing `StyledText`, `StyledBlock`, or full scrolling line
sets.
