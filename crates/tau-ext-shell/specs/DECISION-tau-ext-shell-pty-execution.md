# DECISION-tau-ext-shell-pty-execution: Linux and Android shell output uses PTYs

Authority: confirmed, 2026-07-26, dpc

## Decision

On Linux and Android, commands executed through Tau's shell surfaces attach
stdout and stderr to independent pseudo-terminals while stdin remains closed.
Other targets retain stdout/stderr pipe capture and closed stdin.

## Reason

Shell commands should behave as they do in an interactive terminal. Pipe-backed
execution made programs such as `rustgrep` detect non-TTY capture and select
different, less actionable output even though Tau presents the command as a
shell execution surface. Linux and Android can create every parent-only PTY
endpoint with atomic close-on-exec. Other Unix `openpty` paths require a
create-then-`fcntl` window, so they remain pipe-backed rather than accepting an
inheritance race.
