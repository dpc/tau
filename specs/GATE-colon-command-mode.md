# GATE-colon-command-mode: Colon command mode

## Gate

Tau must use a first-non-whitespace `:` for command mode. A leading `/` must
remain ordinary prompt and path-completion input; Tau must not provide slash
command aliases.

## Justification

The user wants the established Vim, Helix, and tmux command convention and wants
to keep `/` available for path completion without ambiguous command ownership.
