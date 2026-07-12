Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-ext-utils guidance

Read `specs/ARCH-tau-ext-utils.md` before changing timer restore, firing, or display behavior.
Timer state is intentionally active-only and reconstructed from replayed session
execution facts; do not add a separate timer store without an explicit design
change.
