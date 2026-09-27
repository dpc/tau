---
name: tau-self-knowledge-skills
description: Use for creating, discovering, searching, loading, invoking, inspecting, or resolving collisions between Tau Markdown skills.
advertise: false
---

# Using skills

Put a project skill in `.agents/skills/<name>/SKILL.md`, or a user skill in
`~/.config/agents/skills/<name>/SKILL.md`. For example:

```markdown
---
name: example-workflow
description: Use when checking example workflows.
advertise: false
---

# Example workflow
Check the inputs before editing.
```

`name` can default to the directory name; `description` is required. Project
skills default to advertised and user skills to hidden; `advertise` overrides
this. The startup `<available_skills>` advertises *only names and descriptions*,
not bodies. An agent loads a skill with `skill({"query":"example-workflow"})`.
`skill({"query":"workflow tests"})` searches name/description with **OR**
semantics, so more generic words can add results rather than narrow them.
Ambiguous results list candidates; call again with the exact name. Use
`search_content:true` to search the bounded body prefix when necessary.
An initialized agent keeps its frozen skill snapshot even if session discovery
later refreshes; start a new agent to pick up newly discovered skills.

From the terminal, `:skill example-workflow optional text` or
`:skill:example-workflow optional text` explicitly injects a user-invocable
skill into the next prompt. Arguments are appended as text, not substituted
into placeholders. `user-invocable: false` hides this route, while
`disable-model-invocation: true` hides model search/loading and leaves user
invocation enabled. Neither is a security boundary; `allowed-tools` in skill
frontmatter does not control Tau permissions.

For a fresh role's collision-resolved snapshot, use
`tau --role engineer dev print-skills --format json` (`--role` is optional).
This starts configured extensions, which can have persistent or external
side effects, but does not call a model provider. A collision diagnostic
identifies duplicate names: generally the newest available file modification
time wins, with XDG user roots explicitly preferred over alternate
`~/.agents` roots before timestamp comparison. An equal-time tie keeps
the earlier discovered candidate; do not assume the nearest project directory
always wins. Skill reads and body search are bounded to 64 KiB. See
`docs/skills.md` for discovery-root precedence and exact inspection behavior.
