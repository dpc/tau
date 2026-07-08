# tau-ext-utils guidance

Read `ARCHITECTURE.md` before changing timer restore, firing, or display behavior.
Timer state is intentionally active-only and reconstructed from replayed session
execution facts; do not add a separate timer store without an explicit design
change.
