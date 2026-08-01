//! pglord-agent: the instance manager that runs as PID 1 in every Postgres pod.

pub mod backup;
pub mod bootstrap;
pub mod config;
pub mod http;
pub mod initdb;
pub mod metrics;
pub mod params;
pub mod postboot;
pub mod record;
pub mod rolestrategy;
pub mod status;
pub mod supervisor;
