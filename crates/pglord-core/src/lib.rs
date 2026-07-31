//! Shared pglord domain types. Grows into the cluster spec model (see design doc).

pub mod spec;

/// Version of the pglord workspace, surfaced by the `/api/v1/version` endpoint.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
