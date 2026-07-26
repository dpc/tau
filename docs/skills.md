# Skills

Tau discovers Markdown skills into an atomic session baseline, refreshes and
freezes that state for each agent initialization, advertises only the small set
that should be immediately visible, and lets the agent discover or load the rest
with the `skill` tool.


## Discovery

Tau scans skills in priority order:

1. Existing project `.agents/skills` and `.agents.local/skills` directories from the working directory's ancestors, broadest ancestor first and current directory last.
2. `~/.config/agents/skills`
3. `~/.config/agents.local/skills`
4. Legacy `~/.agents/skills`
5. Legacy `~/.agents.local/skills`

When multiple skills use the same name, Tau keeps the candidate with the newest available modification time and reports the conflict as a collision. Skills with readable timestamps beat skills without timestamps. If timestamps are equal or unavailable, the earlier discovered candidate stays selected. User XDG skill roots (`~/.config/agents*`) explicitly beat legacy user skill roots (`~/.agents*`) before modified time is considered. Built-in skills use the harness binary build time as their timestamp, falling back to the executable file mtime when build metadata is unavailable.

Discovery is best-effort and bounded. Tau reads only the bounded frontmatter
prefix needed for metadata, not the whole skill body, and skips a skill with a
diagnostic if its frontmatter does not close before the discovery read limit.
Directory traversal has per-root, per-directory, and depth budgets; exceeding a
budget emits a diagnostic and skips the remaining over-budget traversal.
Symlinked skill roots, symlinked directories, and symlinked Markdown skill files
are followed; Tau tracks canonical directories so symlink cycles do not recurse
forever. Project-controlled skill roots can therefore point discovery at other
local skill files reachable by the user.

Preferred layout:

```text
.agents/skills/<skill-name>/SKILL.md
```

The frontmatter fields Tau reads are:

- `name`: Optional. Defaults to the parent directory name for `SKILL.md`, or to the file stem for a root-level Markdown skill. Must be lowercase ASCII letters, digits, and hyphens only.
- `description`: Required. Used in prompt advertisements, search results, and loaded skill results.
- `advertise`: Optional. `true`, `True`, `TRUE`, and `1` force prompt advertisement. `false`, `False`, `FALSE`, and `0` force no prompt advertisement. Invalid values warn and use the default.
- `user-invocable`: Optional, default `true`. If false, `:skill` rejects the skill and the terminal completion hides it unless `disable-model-invocation: true` overrides it. This does not block model-side invocation.
- `disable-model-invocation`: Optional, default `false`. If true, Tau excludes the skill from `<available_skills>` and from the model-visible `skill` tool, and treats the skill as user-invocable.
- `argument-hint`: Optional short UI hint shown with `:skill` completion.

Tau ignores `allowed-tools` and other provider-specific permission fields; skill frontmatter does not grant or restrict Tau tool permissions.
Project-scoped skills default to advertised. User-scoped skills default to hidden until searched. `advertise:` overrides the scope default.


## Prompt advertisement

Advertised skills from the target agent's frozen snapshot appear in
`<available_skills>` with only name and description. Tau does not include the
skill body until the agent calls `skill`. Later session discovery changes do not
mutate an already initialized agent.

This keeps normal agent context small while still surfacing project-local instructions that are likely relevant immediately.


## User command invocation

Users can force a skill into the next model prompt with either form:

```text
:skill <name> [arguments...]
:skill:<name> [arguments...]
```

The harness validates against the selected agent's frozen skill snapshot, rejects
unknown or non-user-invocable skills with a visible `harness.notice`, reads the
same bounded 64 KiB prefix used by the model-visible tool, strips frontmatter,
and expands the submitted prompt to a Pi-style block. A new agent's initial
`:skill` command waits for its discovery initialization to finalize before this
expansion.

```text
<skill name="..." location="...">
References are relative to ...

...frontmatter-stripped skill body...
</skill>

...opaque arguments, if any...
```

Arguments are append-only text. Tau does not implement placeholder substitution or structured skill arguments.

Terminal `:skill` name completion uses the harness-owned complete canonical
session snapshot. Invocation still validates against the selected agent's frozen
snapshot, which may intentionally differ after later session refreshes.

`disable-model-invocation` and `:skill` visibility are prompt-surface controls, not security boundaries. A model with filesystem tools may still read a skill file if it learns the path, and Tau ignores `allowed-tools` as a permission mechanism.

## The `skill` tool

The agent calls `skill` with a `query` string:

```json
{ "query": "rust style" }
```

Tau lowercases and deduplicates query terms. Punctuation separates terms, except hyphens inside skill names are preserved.

Search uses the target agent's frozen snapshot with OR semantics: a skill matches
if any query term matches its name or description. Hits are sorted by
`matched_terms` descending, then by name. `matched_terms` is the number of
distinct query terms that matched, not an occurrence count.

By default, Tau does not read skill bodies during search. `search_content: true` also searches the first 64 KiB of the skill file after stripping frontmatter from that prefix.

If the query is unambiguous, Tau returns `name`, `description`, full available `content` with frontmatter stripped, and truncation metadata:

- exactly one matching skill was found; or
- the query has one term and one match has exactly that skill name, even if other skills also matched.

Otherwise Tau returns matching skill names, descriptions, `matched_terms`, `matched_fields`, and guidance. For ambiguous results, the agent should usually call `skill` again with only the exact skill name. If searching again, use a more distinctive term; adding generic terms may not narrow results because search uses OR semantics.


## Size limits

Session-start discovery reads a bounded 64 KiB frontmatter prefix for metadata.
If the frontmatter closes inside that prefix, discovery ignores the rest of the
body until the skill is explicitly loaded or searched. If the closing fence is
not found before the limit, the skill is skipped with a diagnostic.

Skill loading and content search read a separate bounded 64 KiB prefix of each skill file. If loading truncates after frontmatter was closed, Tau returns the available body prefix and marks the result as truncated. If truncation happens before the frontmatter closing fence, Tau errors instead of treating YAML frontmatter as skill body.
