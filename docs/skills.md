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
4. `~/.agents/skills`
5. `~/.agents.local/skills`

When multiple skills use the same name, Tau keeps the candidate with the newest available modification time and reports the conflict as a collision. Skills with readable timestamps beat skills without timestamps. If timestamps are equal or unavailable, the earlier discovered candidate stays selected. User XDG skill roots (`~/.config/agents*`) explicitly beat the supported alternate roots (`~/.agents*`) before modified time is considered. Other coding harnesses may store skills in these alternate locations; Tau supports them for interoperability but prefers the XDG locations. Built-in skills use the harness binary build time as their timestamp, falling back to the executable file mtime when build metadata is unavailable.

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
- `disable-model-invocation`: Optional, default `false`. If true, Tau excludes the skill from discovery advertisements and from the model-visible `skill` tool, and treats the skill as user-invocable.
- `argument-hint`: Optional short UI hint shown with `:skill` completion.

Tau ignores `allowed-tools` and other provider-specific permission fields; skill frontmatter does not grant or restrict Tau tool permissions.
Project-scoped skills default to advertised. User-scoped skills default to hidden until searched. `advertise:` overrides the scope default.

## Inspecting effective skills

`tau dev print-skills` initializes a fresh ephemeral agent through the ordinary
discovery lifecycle and prints the advertised-skill portion of the initial
`tau_discovery` user envelope. `tau dev print-agents-md` prints its AGENTS.md
portion. Both use the configured startup role unless `--role ROLE` is set, and
the same discovery and rendering code used for model delivery:

```sh
tau --role engineer dev print-skills
tau --role engineer dev print-agents-md --workdir /path/to/project
```

Use `--format markdown` or `--format json` with `print-skills` to inspect the full
effective inventory, including unadvertised skills. Built-in skills have a `null`
JSON path:

```sh
tau --role engineer dev print-skills --format json
```

Like `print-prompt` and `print-tools`, these developer commands start configured
extensions and may therefore have their ordinary persistent-state side effects.
Neither command calls a model provider. Use `--workdir DIR` to start the preview
harness and agent in that directory for project discovery without changing the
caller's directory. Tau still loads profile configuration from the ordinary
user/XDG locations. These commands show a fresh full initial snapshot, not the
remaining delta of a running agent.


## Prompt advertisement

Advertised skills appear with their names and descriptions in a user-role
`tau_discovery` envelope alongside applicable AGENTS.md instructions, never in
system/developer templates. Delivery waits for the first real inference; loading
or inspecting an agent does not start a model call. Tau does not include a skill
body until the agent calls `skill`.

A persistent workdir change refreshes only that source. New discovery follows the
completed tool-results block as a separate user message in the same continuation,
not inside the tool result or in an extra turn. The scope list identifies current
instructions; previous scopes remain history. Tau references exact versions with
surviving canonical delivery receipts in the current context rather than repeating
their bodies, even after A→B→A or a file-content reversion. Compaction can remove
original receipts, so Tau redelivers needed content on the next writable inference.

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
and expands the submitted prompt to a sentinel-delimited block. A new agent's initial
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

Model-facing generic user-payload framing follows [SPEC-context-output-formatting](../crates/tau-harness/specs/SPEC-context-output-formatting.md#registered-injected-context-envelopes); payload-local XML-like tags do not establish Tau provenance.
