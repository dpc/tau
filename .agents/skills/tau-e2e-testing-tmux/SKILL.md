---
name: tau-e2e-testing-tmux
description: Use Tau's dev tmux helper for manual agent-controlled end-to-end testing of the current checkout.
---

# Tau end-to-end testing with tmux

Use the hidden `tau dev tmux` helper to run a manual Tau session in a private
tmux server with isolated scratch state. This is for agent-controlled manual E2E
checks, not an automated test framework.

1. Build the current checkout:
   ```sh
   cargo build -p tau
   ```
2. Allocate a unique scratch directory so parallel manual workflows do not
   conflict:
   ```sh
   scratch_root="$(mktemp --directory /tmp/tau-e2e-XXXXXX)"
   ```
3. Start an isolated tmux session:
   ```sh
   target/debug/tau dev tmux start \
     --scratch-root "$scratch_root" \
     --tau-bin target/debug/tau
   ```
   By default this sets scratch `HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`,
   `XDG_RUNTIME_DIR`, disables all extensions, and enables only `core-shell`
   with its working directory pointed at scratch `work/`. Use `--workdir` only
   for an existing external directory that should not be created or chmodded by
   the helper.
4. Inspect the UI:
   ```sh
   target/debug/tau dev tmux capture --scratch-root "$scratch_root"
   ```
5. Send input:
   ```sh
   target/debug/tau dev tmux send --scratch-root "$scratch_root" -- /help
   ```
   Add `--no-enter` to paste without submitting.
6. Stop and clean up:
   ```sh
   target/debug/tau dev tmux stop --scratch-root "$scratch_root" --remove-scratch
   ```

Use `--session`, `--width`, and `--height` when you need a non-default session
name or fixed pane size.
