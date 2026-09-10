# Your first Tau task

This is the canonical path from a fresh Tau installation to one reviewed,
resumable repository change. It assumes Linux, a Git repository, and an account
that can use OpenAI Codex through ChatGPT. Tau is early-stage software: native
distribution packaging and broad clean-host qualification are still unfinished.

Read [Trust and data](trust-and-data.md) before using Tau with private source or
credentials. The short version appears below, before the first model request.

## 1. Install Tau

Choose one of the currently available source-based routes.

### Try with Nix

With a Nix installation that has flakes and the `nix` command enabled:

```sh
nix run github:dpc/tau -- --version
```

This runs Tau through its flake; it does not install a persistent `tau` command.
In the remaining examples, replace `tau` with
`nix run github:dpc/tau --` if you use this route.

### Install with Cargo

This route requires Rust 1.97 and the native build tools needed by the
dependencies:

```sh
cargo install --locked --git https://github.com/dpc/tau --package dpc-tau
tau --version
```

The installed binary includes the CLI, harness, built-in providers, and standard
extensions. NixOS is not required, but Tau is Unix-first and the default
restricted extension startup requires Linux 5.12 or later. The
[native packaging tools](../packaging/README.md) currently produce unqualified
candidates, not published native distributions.

## 2. Create the starter configuration

```sh
tau init
```

This creates starter files under `${XDG_CONFIG_HOME:-$HOME/.config}/tau/`.
It does not overwrite existing files unless you explicitly force it. If
configuration already exists, review it rather than replacing it.

## 3. Understand the boundary before inference

For this first durable session:

- Tau's shell and filesystem tools can read and change files available to their
  configured process. A prompt such as “do not edit yet” is an instruction to
  the model, not an enforced write barrier.
- Prompts, selected file content, tool results, and other request context go to
  the selected inference provider. Network tools can also send queries or
  fetched URLs to their configured services.
- Configured extensions are trusted same-user executables, not hostile-code
  sandboxes. Persistent supervised extensions can read other Tau state by
  default, although Tau mounts that state read-only and supports an explicit
  `hidden` policy.
- Durable sessions and agent transcripts remain in Tau's state directory.
  Cleanup for sessions and agents is disabled by default. Exact provider
  request/response capture is also on by default for durable activity, under
  owner-private session storage; diagnostic cleanup defaults to 30 days.

See [Trust and data](trust-and-data.md) for paths, controls, and the limits of
Tau's isolation language.

## 4. Authenticate a provider

The walkthrough uses Tau's ChatGPT/Codex backend:

```sh
tau provider add chatgpt
tau provider list
```

Follow the browser login flow with your own eligible ChatGPT account. Account
and model availability, workspace policy, and usage limits are controlled by
OpenAI and can vary. This route uses ChatGPT/Codex access; it is not the same as
OpenAI API-key billing. OpenAI documents
[Codex plan access](https://help.openai.com/en/articles/11369540-using-codex-with-your-chatgpt-plan)
and [ChatGPT/API billing](https://help.openai.com/en/articles/9039756-managing-billing-settings-on-chatgpt-web-and-platform)
separately.

If the profile already exists but its OAuth credential is missing or expired,
run the exact `tau provider login <profile>` command shown by
`tau provider list`. Provider settings contain credential references, not the
OAuth tokens themselves. The detailed storage and login contract is in
[Providers](providers.md#scoped-provider-credentials).

## 5. Choose a safe repository task

Start with a clean or disposable Git branch/worktree. Do not use a repository
with unrelated uncommitted work for this exercise.

```sh
cd /path/to/repository
git status --short
tau
```

Tau prints the new session ID and selected model during startup. Press
<kbd>Ctrl-V</kbd> if you want to switch between the compact conversation and
full tool transcript.

Ask Tau to inspect before changing anything:

> Read the repository instructions. Find one small, low-risk documentation or
> test improvement. Propose exactly one change and the narrowest relevant check.
> Do not edit files yet.

Review the proposal. If it is appropriately bounded, continue:

> Make only that proposed change. Run the narrowest relevant check. Do not make
> unrelated changes. Summarize the files changed and the exact commands and
> results.

This two-step prompt is a review habit, not a security boundary. Cancel or
reject a proposal that is vague, broad, destructive, or needs credentials you
did not intend to expose.

## 6. Inspect the result yourself

In another terminal, from the same repository:

```sh
git status --short
git diff --check
git diff
```

Run the focused check yourself using the command Tau reported. Inspect both the
diff and the command output; a model summary is not evidence that either is
correct. Revert or edit the change with your normal tools if needed.

## 7. Stop, detach, and resume

Inside Tau:

- `:quit` (or `:q`) leaves the UI and normally terminates a foreground-owned
  session after the last UI disconnects.
- `:detach` leaves the daemon running so work can continue.
- `:quit-session` explicitly requests shutdown of the session daemon.

After a normal `:quit`, resume the saved session:

```sh
cd /path/to/repository
tau resume
```

With one unlocked saved session Tau selects it; otherwise it opens a picker.
You can also use `tau resume SESSION_ID`. After resuming, ask:

> Recap the change, the check that ran, and anything still unresolved. Do not
> make another change.

Use `tau attach [SESSION_ID]` instead when the daemon is still running. See
[Session startup](session-startup.md) for the exact attach, detach, resume, and
shutdown lifecycle.

## If a step fails

Use the first matching branch:

1. **The command is missing or does not start:** rerun the install route's
   version command. Cargo users should confirm that Cargo's binary directory is
   on `PATH`.
2. **Configuration fails:** read the exact startup error and inspect
   `${XDG_CONFIG_HOME:-$HOME/.config}/tau/`. Do not run `tau init --force` or
   delete state as a generic repair.
3. **No provider or model is available:** run `tau provider list`. Repair an
   expired ChatGPT login with the displayed `tau provider login` command, then
   restart Tau. Authentication success does not guarantee that a particular
   model is available to the account.
4. **Restricted extension startup fails:** confirm Linux 5.12 or later and read
   the mount error. Tau deliberately fails closed rather than weakening the
   recursive read-only state mount.
5. **Attach or resume fails:** use `tau session list` outside Tau's supervised
   shell to find live daemons. `attach` needs a live daemon; `resume` needs
   valid persisted state.
6. **A tool or model turn fails:** preserve the short error and note the session
   ID. Owner-private logs are under the session directory, but review them
   before sharing: provider captures can contain prompts, tool data, paths,
   account metadata, or provider-controlled content.

For deeper local inspection, see Tau's
[debugging self-help](../crates/tau-skills/self-knowledge/tau-self-knowledge-debugging.md).
When asking for help, start with the Tau version, install route, OS/kernel and
architecture, failing step, exact safe error, and whether a minimal
configuration reproduces it. Do not upload the whole Tau state directory or a
provider capture by default.

## Upgrade conservatively

An already-running daemon keeps using its existing binary and startup
configuration. Before replacing Tau:

1. Finish or stop active work and use `:quit-session` for daemons you intend to
   replace.
2. Record `tau --version`.
3. Preserve the current executable in a private backup location, or record the
   exact source revision/install reference needed to reproduce it.
4. Back up `${XDG_CONFIG_HOME:-$HOME/.config}/tau/` and
   `${XDG_STATE_HOME:-$HOME/.local/state}/tau/` while Tau is stopped.
5. Repeat your install route. Cargo users can add `--force` to the install
   command when replacing an existing binary.
6. Run `tau --version`, then `tau resume SESSION_ID`.

Tau does not promise backward compatibility while under heavy development. Do
not delete persisted state to fix an upgrade failure; keep the old binary and
backup until you have inspected the error and recovered the work you need.
