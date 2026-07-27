# GATE-tau-harness-system-prompt-templates: Assemble system prompts through templates

## Gate

Every dynamic system-prompt value must enter the rendered prompt through an
explicit template input. Code must not insert, replace, prepend, append, or
otherwise edit system-prompt content around template rendering; processing that
is strictly transport-only may remain outside the template.

## Justification

The user wants custom templates to control the placement and wording of every
dynamic value instead of being silently overridden by prompt string surgery.
