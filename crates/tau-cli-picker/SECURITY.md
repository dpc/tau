# tau-cli-picker security and reliability notes

`tau-cli-picker` is a local synchronous terminal picker. It has no network,
persistence, subprocess, secret storage, or model-facing policy boundary.

## Terminal ownership

`pick` and `pick_with_writer` enable raw mode and own restoration for the
lifetime of the call. They must not be embedded in another TUI or caller that
already owns raw mode: restoring cooked mode would break the parent terminal
session. Hosts that already own terminal mode should use `pick_with_io` only for
simple byte-stream tests/flows, or add a future API that accepts host-owned
events and sizing explicitly.

Raw-mode restoration errors are reported from raw-mode-owning entry points, even
when the picker otherwise selected, cancelled, or hit an input/rendering error,
so a caller is not told the terminal was safely restored when it may still be
raw. A
`Drop` fallback still performs best-effort restoration if the explicit restore
path fails or unwinding skips it.

## Inputs and output

Picker labels and prompts are treated as display text, not trusted terminal
control data. Rendering goes through `tau-term-screen` styled-cell output and
truncation rather than writing labels directly as terminal commands.

The picker returns only the selected item index. It should not log prompts,
labels, or user input, and callers should avoid placing secrets in visible
picker labels because they are written to the terminal.

## Cleanup expectations

Successful selection must clear the rendered picker frame and report cleanup
failures. Cancellation and input errors keep their original picker error while
frame cleanup is best-effort. Resize handling erases the current frame,
invalidates cached screen state, and redraws with bounded dimensions.
