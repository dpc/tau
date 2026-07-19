# DECISION-instruction-symlink-discovery: Follow symlinks in trusted instruction discovery

Authority: confirmed, 2026-07-03, user

Tau follows symlinks while discovering trusted AGENTS files, skill roots,
directories, and Markdown skill files. This supports ordinary dotfile,
shared-repository, and project-local linked layouts.

This is an accepted prompt trust-boundary choice, not a sandbox escape: these
files are already trusted instructions. Traversal and reads remain bounded and
canonical-directory tracking breaks cycles. The tradeoff is that a trusted root
may reach linked prompt content elsewhere, so repositories and skill roots must
contain only trusted instructions.
