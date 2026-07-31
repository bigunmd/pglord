//! Shared pglord domain types. Grows into the cluster spec model (see design doc).

pub mod guc;
pub mod render;
pub mod spec;

/// Version of the pglord workspace, surfaced by the `/api/v1/version` endpoint.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Annotation on the spec ConfigMap naming the pod currently allowed to accept
/// writes. Written only by the server (CAS); read by server and agents.
pub const PRIMARY_ANNOTATION: &str = "pglord.dev/primary";
