# REQ-context-file-frontmatter-fail-open: Keep malformed context files visible

## Requirement and source

The user explicitly requires skills and AGENTS.md files with malformed filters
or headers to remain visible rather than silently lose useful instructions.
Ignore all four role/group visibility filters when any recognized filter is
invalid. Preserve valid unrelated metadata. When a header cannot be parsed,
preserve the available raw instructions rather than guessing metadata or
discarding the header region.

Every malformed file must produce a file-specific corrective warning in the UI.
Warnings must remain visible under diagnostic filtering and to users attaching
after discovery. A log entry or best-effort transient diagnostic is insufficient.
Recovery must not bypass bounded reads or manufacture unavailable file contents.
The user explicitly permits discarding a skill when its essential identity
cannot be recovered from either its header or existing path-derived name, or
when its contents cannot be loaded. Do not invent aliases or inject such files
as bootstrap instructions. An unidentifiable skill still requires a file-specific
corrective UI warning; auxiliary metadata faults alone must not hide recoverable
skills or AGENTS instructions.

## Justification

These context files are not generally sensitive. Showing unintended instructions
usually wastes context rather than creating a security problem; silently hiding
them makes configuration mistakes harder to notice and repair. Role filtering
is context selection, not a filesystem access-control or confidentiality boundary.

Discovery and frozen initialization are governed by
[SPEC-session-discovery-declarations-and-readiness](SPEC-session-discovery-declarations-and-readiness.md).
