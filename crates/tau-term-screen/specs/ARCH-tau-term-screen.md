# ARCH-tau-term-screen: tau-term-screen architecture

`tau-term-screen` is a synchronous terminal layout and rendering library. It does
not own terminal input, process spawning, networking, persistence, or background
tasks; callers own those boundaries and must bound the amount of text they ask the
crate to lay out or render.

`PriorityLine` provides reusable single-row progressive layout for styled
presentation elements. It measures terminal display columns, preserves stable
item order within left/right edge groups, emits the left group before the right,
and removes larger integer priorities until retained items, separators, and
minimum inter-group padding fit. An item may instead opt into bounded middle
truncation with inclusive minimum and maximum display widths. The allocator
reserves every truncatable survivor's minimum, drops whole items by priority
when those minima do not fit, then assigns remaining columns toward configured
maxima in ascending priority and insertion order. Truncation preserves complete
graphemes and styles around the exact `┄` marker. Equal priorities remove later
inserted items first, empty items are ignored, and explicitly attached fragments
omit their normal intra-group separator.

A caller may require an essential priority band to survive together; if any
accepted item in that band cannot fit, the line stays empty instead of showing
an incomplete meaning. The same empty fallback applies when the
highest-importance survivor cannot fit after less-important items are removed,
rather than resurrecting a smaller item, wrapping, or clipping content.
`StyledBlock::priority_line` selects this layout instead of ordinary and
right-adornment content. Its optional priority-line body supports related
ordinary detail rows without letting the adaptive header wrap; when an
essential header band cannot fit, the owned body hides with it so detail rows
never lose their identity/status context. Ordinary
`StyledBlock::right_content` remains the atomic right-adornment mechanism used
by the multi-line-capable prompt.

## Terminal output trust boundary

Caller-provided text is untrusted with respect to terminal control sequences.
Styles are represented as structured values, not inline ANSI bytes. Layout and
cell emission sanitize tabs and non-newline control characters before output so
styled text cannot inject arbitrary terminal controls such as clears, cursor
moves, or OSC sequences.

Newline graphemes are layout separators, not inline cells. Hand-built public
`Cell` values are normalized again before diffing/caching and before emission so
callers cannot bypass the styled-text sanitizer by constructing cells directly.

OSC 8 hyperlinks are structured span/cell metadata rather than inline text.
Layout preserves that metadata across wrapping, while emission opens and closes
each contiguous linked cell range. Targets containing control characters are
rejected, targets longer than 4096 bytes are omitted to keep wrapped output
amplification bounded, and link labels pass through the ordinary text sanitizer.

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
