# ARCH-tau-skills: tau-skills architecture

Skills are prompt instructions loaded from local/project Markdown files, not a sandbox or permission boundary. Project skills can be malicious prompt content. `disable-model-invocation` hides a skill from Tau's model-visible skill surfaces, but a model with filesystem tools could still read the underlying file if it learns the path. `allowed-tools` and similar frontmatter fields do not grant or restrict Tau tool permissions.

Skill discovery is best-effort and bounded. It reads only a bounded
frontmatter prefix during session startup, skips and diagnoses skills whose
frontmatter does not close inside that prefix, and bounds directory traversal by
explicit per-root, per-directory, and depth budgets. Exceeding a traversal budget
emits a diagnostic and skips the remaining over-budget traversal. Symlinked
skill roots and entries are followed while tracking canonical directories to
avoid recursion cycles. Project-controlled skill roots can therefore point
discovery at other local skill files reachable by the user; do not treat skill
discovery as a sandbox.

`agents.required_skills` and group/role `required_skills` are fail-closed
availability checks, not permission boundaries. They only require that exact
skill names are discoverable and model-loadable before a role may be selected or
delegated; they do not make skill content trusted, restrict filesystem access to
the skill file, or grant tools mentioned by skill frontmatter.

User `/skill` invocation explicitly reads the selected skill file, strips frontmatter, and injects the skill body into the next model prompt along with any user arguments. Treat invoking a skill as intentionally adding that local file content to the conversation context.
