//! User configuration loading.
//!
//! The crate's layering and validation boundaries are summarized in
//! `ARCH-tau-config`.
//!
//! Settings live in `~/.config/tau/` as YAML: `cli.yaml` and
//! `harness.yaml`, each with an optional `*.d/*.yaml` drop-in directory
//! for layered overrides. See
//! [`settings`] for the schema and loader entry points.
//!
//! Resolved-harness types and the user-vs-builtin extension resolver
//! live in `tau-harness` — this crate just owns the on-disk schema.

pub mod atomic;
pub mod provider_debug_capture;
pub mod settings;
