---
name: tau-self-knowledge-prompt-templating
description: Use when customizing Tau prompt templates or fragments, including Handlebars variables, prompt fragment priorities, role prompt customization, project-specific prompt conditionals, or system prompt template overrides.
advertise: false
---

# Tau prompt templating

System-prompt templates and their dynamic inputs are separate from Tau's generic
user-role transcript carrier. New or reframed free-form payloads in that carrier
follow
[`GATE-new-generic-user-payload-envelopes`](../../../specs/GATE-new-generic-user-payload-envelopes.md)
and
[`SPEC-context-output-formatting`](../../tau-harness/specs/SPEC-context-output-formatting.md#registered-injected-context-envelopes);
template authors must not invent local provenance wrappers for them.

Tau uses strict Handlebars: unknown variables prevent prompt dispatch.
Guard optional data with `cwd` or `working_directory.present`.

Context-size alert `message` uses the same data/helpers and owning prompt
snapshot. Whitespace-only output omits an alert; render errors prevent dispatch.

HTML escaping is off. Use `xml_escape` for XML-shaped sections.

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

Set `textFile` instead of `text` to load the same Handlebars template source
from a UTF-8 file:

```yaml
agents:
  prompt_fragments:
    - name: user.long-policy
      priority: 65
      textFile: prompt-fragments/long-policy.hbs
```

`text` and `textFile` are mutually exclusive. Relative paths start at the
directory containing `harness.yaml`, including from `harness.d` or CLI layers.
Unreadable/non-UTF-8 files fail config loading. Disabled roles' fragments still
load; unselected profiles' files do not. File text uses inline rendering,
additive layering, and full-fragment de-duplication.

Fragment content enters model prompts and applicable diagnostics; do not put
secrets in it.

`agents.prompt_fragments` apply globally, including one-shot config overrides;
group/role fragments apply at that scope. New groups should define `roles:`;
group fragments without roles mainly override built-ins. Ascending `priority`
orders fragments; values below `100` precede generated sections such as skills.

Tau includes its role catalog only when `agent_start` is available.
Whitespace-only fragments are omitted from `prompt_fragments`.

Extensions can publish transient runtime fragments through interception.
Disconnect removes their fragments; late subscribers receive no replay.

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

Full templates also receive `agent_id` for a concrete agent, plus rendered
`prompt_fragments` and `tool_prompt_fragments` with `name`, `priority`, `content`,
and `early`. Built-ins omit `agent_id`; agents can use `self_info({})`.
Tool fragment content includes ``### `<tool>` instructions``.
Render optional `payload_envelope_provenance_notice` once when present; omitting
it removes Tau's model-visible provenance cue for stamped payload envelopes.

Consumers may capability-gate extension fragments. `shell.workdir` requires
`shell:workdir` in the effective tool snapshot; multiple shell instances
coalesce into one fragment.

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
