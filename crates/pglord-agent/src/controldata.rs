//! Thin wrapper over `pg_controldata`: the pure parsing lives in
//! pglord_core::standby so it can be unit-tested without a PGDATA.

use std::path::Path;

use pglord_core::standby::ControlFileValues;
use tokio::process::Command;

/// The primary's live hot-standby settings as tracked by this PGDATA's
/// control file. None (with a warning) when pg_controldata is missing or
/// its output is unreadable — the caller then applies no clamp, which is
/// exactly the pre-clamp behavior.
pub async fn live_values(pgdata: &Path) -> Option<ControlFileValues> {
    let out = match Command::new("pg_controldata")
        .arg("-D")
        .arg(pgdata)
        .output()
        .await
    {
        Ok(out) if out.status.success() => out.stdout,
        Ok(out) => {
            tracing::warn!(status = %out.status, "pg_controldata failed; skipping hot-standby clamp");
            return None;
        }
        Err(err) => {
            tracing::warn!(%err, "pg_controldata not runnable; skipping hot-standby clamp");
            return None;
        }
    };
    let text = String::from_utf8_lossy(&out);
    let parsed = pglord_core::standby::parse_controldata(&text);
    if parsed.is_none() {
        tracing::warn!("pg_controldata output unparseable; skipping hot-standby clamp");
    }
    parsed
}
