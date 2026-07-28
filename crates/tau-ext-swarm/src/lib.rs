//! In-memory Tau session projection and Tau Swarm application adapter.

mod application;
mod config;
mod projection;
mod runtime;
mod tools;

pub use runtime::{run, run_stdio};
