# tau-cli-picker architecture

`tau-cli-picker` is a small synchronous picker for short CLI selection flows. It owns picker-local state only: the selected item, the visible item window, the current terminal size sample, and cleanup of the frame it rendered.

## Terminal ownership

The crate exposes three entry points with explicit ownership contracts:

- `pick` enables raw mode and renders to `stderr`.
- `pick_with_writer` enables raw mode and renders to a caller-provided writer.
- `pick_with_io` does not manage raw mode and is intended for tests or simple byte-stream hosts.

Do not call the raw-mode entry points from a parent TUI that already owns raw mode. Dropping the picker guard disables raw mode and would leave the parent in cooked mode. Embedded TUI use needs a separate API that accepts host-provided events, resize notifications, and size samples.

## Input model

Terminal events and byte-stream input both funnel through a shared logical key map. Keep key behavior centralized so tests can protect parity between `crossterm` events and byte-stream fixtures.

The picker recognizes Enter for selection; Up/Down, `k`/`j`, and BackTab/Tab for navigation; Escape, Ctrl-C, Ctrl-D/EOF, and `q` for cancellation. Space is intentionally ignored and reserved for possible future multi-select behavior.

## Rendering and resize behavior

Rendering builds a pure frame of styled rows and passes it to `tau-term-screen::Screen`, which performs terminal diffing. The prompt occupies the first row when there is room, and the selected enabled item is kept in a centered visible window. A one-row terminal uses a compact prompt-and-item frame so the picker does not intentionally render more rows than the reported height.

Resize events are part of the picker event stream. A resize erases the current frame, invalidates the screen cache, updates dimensions, and redraws immediately instead of waiting for another keypress.

## Cleanup and errors

Successful selection clears the picker frame and returns cleanup I/O errors. Cancellation and input errors preserve the original user-facing error; cleanup on those paths is best-effort. Raw-mode-owning entry points explicitly restore cooked mode before returning; raw-mode restoration failures are reported as I/O errors even if the picker otherwise selected, cancelled, or encountered an input/rendering error so callers are not told terminal ownership was safely released when it was not.

## Testing strategy

Keep key-map tests close to `key.rs`, where terminal events and byte-stream
bytes are converted into logical picker actions. Picker-loop tests belong in
`src/tests.rs` and should cover navigation, validation, cancellation, cleanup,
resize redraws, and raw-mode restoration precedence through injected readers,
writers, and guards. Rendering tests should assert pure `picker_lines` output so
layout behavior stays deterministic without requiring a live terminal.

## Non-goals

This crate should not grow general TUI concepts such as async event loops, background redraw threads, nested widgets, global application actions, or long-lived terminal ownership. Add those only after designing a public API that models host terminal ownership explicitly.
