# DECISION-instruction-symlink-discovery: Follow symlinks in trusted instruction discovery

Authority: confirmed, 2026-07-03, user

Tau follows symlinks while discovering and loading trusted prompt inputs:
AGENTS.md files, AGENTS.*.md files, skill roots, skill directories, and Markdown
skill files. This supports ordinary dotfile, shared-repository, and project-local
layouts where instructions or skill collections are linked from another location.

This is an accepted prompt trust-boundary choice, not a sandbox escape. These
files are already trusted instructions capable of steering the agent, so refusing
symlinks would mostly break legitimate layouts without creating a meaningful
security boundary. Implementations still bound traversal and reads, track
canonical directories to break cycles, and document that repositories and skill
roots must contain trusted prompt content.
