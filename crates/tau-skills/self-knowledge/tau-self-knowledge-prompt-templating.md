---
name: tau-self-knowledge-prompt-templating
description: >
  Use this skill when the user asks about Tau prompt templates, prompt fragments,
  Handlebars variables, prompt fragment priorities, role prompt customization,
  project-specific prompt conditionals, or system prompt template overrides.
advertise: false
---

# Tau prompt templating

Tau renders prompt fragments and system prompt templates with Handlebars in strict mode. Unknown variables make that fragment/template fail to render, so prefer documented variables and guard optional data with stable fields like `cwd` or `working_directory.present`. A bad prompt fragment is skipped; a bad custom system prompt template falls back to Tau's built-in system template.

Templates are plain prompt text, not HTML. Tau disables default HTML escaping so paths and shell snippets render exactly. Use `xml_escape` only when inserting text into XML-shaped prompt sections.

## Where templates are configured

`~/.config/tau/harness.yaml` and `harness.d/*.yaml` can define prompt fragments:

```yaml
agents:
  role_groups:
    engineer:
      prompt_fragments:
        - name: engineers.review-requirement
          priority: 65
          text: |-
            ### Code review

            If your task involved code changes to a larger project, consider them work in progress until they pass review.
```

`agents.prompt_fragments` apply to every role in every role group, including fragments supplied by one-shot harness config overrides. Role-level `prompt_fragments` apply only to that role or role group. Group-level fragments without `roles:` are mainly useful for overriding an existing built-in group; new groups should define roles. Fragments are sorted by ascending `priority`; priorities below `100` render before later generated system-prompt sections such as skills.

Tau's built-in global fragment lists available agent roles for `agent_start`
whenever the effective prompt tool surface includes that tool. Tau omits it
from prompt template data for roles or models without it.
Ordinary prompt fragments whose templates render only whitespace are likewise
omitted from the `prompt_fragments` array exposed to custom system templates.

Configured extensions can also publish runtime prompt-fragment declarations. Tau
commits those transient events through interception before changing prompt
assembly, removes a contributor's fragments when it disconnects, and does not
replay them to late subscribers.

Roles can also choose a full system prompt template with `prompt_override`; custom templates live under `~/.config/tau/prompts/<name>.hbs`.

## Variables for prompt fragments

Prompt fragments can use:

- `role.name` — current role name.
- `role.group` — stable configured role-group name from the
  `agents.role_groups.<name>` key containing the current role. For an ungrouped
  role, this equals `role.name`.
- `session.cwd` — canonical current directory captured when the harness started.
  This session-wide startup path does not change when an agent changes its shell
  workdir.
- `cwd` — durable agent working directory as a string, or `""` when rendering without a target agent.
- `working_directory.present` — boolean indicating whether `cwd` is available.
- `working_directory.path` — same path as `cwd`.
- `working_directory.basename` — final path component.
- `working_directory.ancestors` — array ordered from the working directory up to the filesystem root.
- `skills` — prompt-visible skills, with `name`, `description`, and `baseDir`.
- `agent_context` — extension-published per-agent context, keyed by context name. Each key is an array of contributions with `extension_name` and `value`.

Full system prompt templates additionally receive `agent_id` for a concrete
agent plus rendered `prompt_fragments` and `tool_prompt_fragments` arrays. Tau's
built-in templates intentionally omit `agent_id`; custom templates retain it for
explicit placement, while agents can query current authoritative identity with
the input-free `self_info({})` tool. Each fragment item has `name`, `priority`,
`content`, and `early`. Tool prompt fragment `content` already includes Tau's
automatic ``### `<tool>` instructions`` heading.
They also receive optional `exact_sentinel_boundary_rule` text whenever selected
context contains a Tau-stamped payload envelope. Custom templates should render
that rule once; omitting it replaces Tau's model-visible provenance cue.

Extension fragments can also be capability-gated by their consumer. In
particular, Tau includes the shared `shell.workdir` fragment only when the
effective role/model tool snapshot contains a `shell:workdir` capability, and
coalesces declarations from multiple shell instances into one rendered
fragment.

## Helpers

Tau registers these helpers:

- `sort` — sorts arrays; use `by="field"` for arrays of objects.
- `trim` — trims rendered text.
- `xml_escape` — XML-escapes rendered text.
- `eq` — returns true when two values compare equal.
- `starts_with` — returns true when a string starts with a prefix.

## Examples

Project-specific prompt fragment:

```yaml
agents:
  prompt_fragments:
    - name: project.rust-extra
      priority: 80
      text: |-
        {{#each working_directory.ancestors}}
        {{#if (eq this "/home/dpc/lab/tau-agent")}}
        ### Tau project rules

        Prefer `jj` change IDs when referring to commits.
        {{/if}}
        {{/each}}
```

Exact-directory conditional:

```yaml
agents:
  prompt_fragments:
    - name: project.root-only
      priority: 80
      text: |-
        {{#if (eq working_directory.path "/home/dpc/lab/tau-agent")}}
        You are at the Tau repository root.
        {{/if}}
```

Session-directory conditional:

```yaml
agents:
  prompt_fragments:
    - name: project.session-root
      priority: 80
      text: |-
        {{#if (eq session.cwd "/home/dpc/lab")}}
        The harness started in the lab directory.
        {{/if}}
```

Role-specific style fragment:

```yaml
agents:
  role_groups:
    user:
      roles:
        assistant:
          prompt_fragments:
            - name: assistant.personal
              priority: 65
              text: |-
                You are a personal assistant.

                Help the user manage email, calendars, TODO lists, and approved actions.
```

Skill listing fragment:

```yaml
agents:
  prompt_fragments:
    - name: debug.skills
      priority: 110
      text: |-
        Available prompt skills:
        {{#each (sort skills by="name")}}
        - {{name}}: {{description}}
        {{/each}}
```
