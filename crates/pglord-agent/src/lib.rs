//! pglord-agent: the instance manager that runs as PID 1 in every Postgres pod.

pub mod bootstrap;
pub mod config;
pub mod http;
pub mod rolestrategy;
pub mod status;
pub mod supervisor;
