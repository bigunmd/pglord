//! Shared pglord domain types. Grows into the cluster spec model (see design doc).

pub mod backup;
pub mod deps;
pub mod env;
pub mod guc;
pub mod init;
pub mod render;
pub mod restore;
pub mod spec;
pub mod workload;

/// Version of the pglord workspace, surfaced by the `/api/v1/version` endpoint.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Annotation on the spec ConfigMap naming the pod currently allowed to accept
/// writes. Written only by the server (CAS); read by server and agents.
pub const PRIMARY_ANNOTATION: &str = "pglord.dev/primary";
