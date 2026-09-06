# dpc-tau-actions

`dpc-tau-actions` defines the serializable action schemas published by Tau
extensions and parses command-mode action invocations.

The crate stays dependency-light so extensions, clients, and the Tau harness
can share one deterministic validation and parsing contract.

This crate's Rust API follows its Cargo package version. It is separate from
Tau's harness-extension protocol revision.
