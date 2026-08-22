mod application;
mod config;
mod delivery;
mod events;
mod lifecycle;
mod matching;
mod models;
mod providers;
mod routes;
mod runtime;
mod source_registry;
mod storage;
mod subscriptions;
mod utils;

pub use application::run_from_env;

#[cfg(feature = "benchmarks")]
pub mod benchmark_support;
