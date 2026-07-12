# DESIGN-instruction-symlink-discovery: AGENTS.md and skill discovery follow symlinks

Status: confirmed, 2026-07-03, user

Tau intentionally follows symlinks while discovering and loading trusted prompt
inputs: AGENTS.md files, AGENTS.*.md files, skill roots, skill directories, and
Markdown skill files. This supports normal dotfile, shared-repository, and
project-local layouts where instruction files or skill collections are linked
from another location.

This is an accepted prompt trust-boundary decision, not a sandbox escape. These
files are already trusted instructions that can steer the agent once loaded, so
refusing symlinks would mainly break legitimate layouts without creating a
meaningful security boundary. Implementations must still bound traversal and
reads, track canonical directories for skill traversal so symlink cycles cannot
recurse forever, and document that users should only run Tau in repositories and
skill roots whose prompt content they trust.
