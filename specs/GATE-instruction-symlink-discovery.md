# GATE-instruction-symlink-discovery: Follow symlinks in trusted instruction discovery

## Gate

Tau must follow symlinked AGENTS files, skill roots, directories, and Markdown
skill files during trusted instruction discovery.

## Justification

The user wants ordinary dotfile, shared-repository, and project-local linked
layouts to work. These paths already contain trusted instructions; bounded
traversal and canonical-directory cycle detection control discovery cost.
