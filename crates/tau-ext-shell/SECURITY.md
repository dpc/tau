# tau-ext-shell security and reliability notes

`tau-ext-shell` executes local commands and mutates local files with the user's
permissions. Directory update locks are advisory coordination for Tau/ext-shell
tools, not an operating-system sandbox or access-control boundary.

User-facing `!` / `!!` shell commands stream bounded progress events separately
from their bounded final captured output. After the per-stream progress cap is
hit, ext-shell keeps draining child pipes for process liveness and final
truncation metadata but stops forwarding arbitrary output volume into the event
stream.

The filesystem directory-lock backend coordinates multiple ext-shell instances
for the same host and user through a private local registry. The registry
directory must be private (`0700` on Unix) when explicitly configured; unsafe
initialization fails closed as a configuration error rather than silently falling
back to process-local locking.

Each filesystem-backend instance holds an exclusive lease lock. Other instances
may reap registry records only after that lease is no longer held; timestamps are
used for abandoned-lock diagnostics, not process liveness. Automatic lock guards
keep the lease and release handle that granted them alive until the running tool
drops the guard, including across backend reconfiguration.

The filesystem backend remains advisory. If ext-shell exits while a spawned shell
descendant deliberately detaches and keeps mutating files, the lease is released
and another Tau instance may proceed while that detached process is still
running.

AGENTS.md discovery and skill discovery are trusted implicit prompt input. The
shell extension intentionally follows AGENTS.md, `.agents.local`, skill-root,
skill-directory, and Markdown skill-file symlinks in both user and project
locations. This is an accepted prompt trust-boundary choice, not a filesystem
sandbox: do not run Tau in repositories whose AGENTS.md files or skills you do
not trust.
