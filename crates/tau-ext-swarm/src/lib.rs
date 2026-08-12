//! In-memory Tau session projection and Tau Swarm application adapter.

mod application;
mod config;
mod projection;
mod runtime;
mod tools;
mod worker_health;

#[cfg(test)]
mod worker_health_tests;

pub use runtime::{run, run_stdio};
