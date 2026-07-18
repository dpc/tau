# DECISION-tau-harness-system-prompt-templates: System prompts are assembled only through templates

Authority: confirmed, 2026-06-17, dpc

System prompts must be assembled through the prompt templating system. Any new
dynamic system-prompt value must be an explicit template variable/input, not text
formatted, prepended, appended, replaced, or otherwise edited around rendered
prompt content.

Ad-hoc string surgery for prompt variables such as `agent_id` is forbidden both
before and after template rendering. Exceptions are only for clearly documented
transport concerns that are not system-prompt content. This keeps custom
templates in control of placement and wording for dynamic values.
