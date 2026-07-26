# DECISION-tau-ext-shell-non-interactive-pager-environment: Shell pagers are disabled by default

Authority: confirmed, 2026-07-26, dpc

## Decision

Tau must protect non-interactive model and user shell commands from pager
prompts by overriding `PAGER`, `GIT_PAGER`, `GH_PAGER`, `JJ_PAGER`, and
`SYSTEMD_PAGER` with `cat` at the shared spawn boundary. This protected overlay
applies after the inherited environment and ordinary shell environment
configuration.

The default applies consistently to model tool calls and user `!` / `!!`
commands. One explicit configuration opt-out may disable the overlay, but doing
so forfeits Tau's non-interactive pager protection for these tools.

Tau must preserve `TERM` and the existing output-PTY, closed-stdin, process
isolation, termination, bounded-drain, and output-fidelity contracts.

## Rationale

TTY-style stdout and stderr select useful terminal output, but they can also
select a pager that cannot read from Tau's closed stdin and therefore stalls.
Protected pager variables prevent that conflict without discarding TTY-style
output. `TERM=dumb` is not pager protection and can undermine that output.

`JJ_PAGER` is protected because jj gives it precedence over `PAGER`, and the
reported failure was a jj command. Other tool-specific variables such as
`MANPAGER` and `BAT_PAGER` remain ordinary without a demonstrated failure;
protecting a focused set does not claim that arbitrary programs cannot prompt.
