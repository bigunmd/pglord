//! Postgres process supervision. The agent is PID 1; postgres is its child.
//! SIGTERM to the agent triggers a fast shutdown of postgres and a clean exit.
//! If postgres dies on its own, the agent exits with its status so the pod
//! restarts (Kubernetes owns restart policy, not the agent).

use anyhow::Context;
use tokio::process::{Child, Command};

use crate::config::Config;

pub fn spawn_postgres(cfg: &Config) -> anyhow::Result<Child> {
    tracing::info!(pgdata = %cfg.pgdata.display(), "starting postgres");
    Command::new("postgres")
        .arg("-D")
        .arg(&cfg.pgdata)
        .kill_on_drop(true)
        .spawn()
        .context("spawn postgres")
}

/// Runs until postgres exits or a shutdown signal arrives.
/// Returns postgres' exit code (0 for a signalled clean shutdown).
pub async fn supervise(mut child: Child) -> anyhow::Result<i32> {
    tokio::select! {
        status = child.wait() => {
            let status = status.context("wait for postgres")?;
            tracing::warn!(%status, "postgres exited on its own");
            Ok(status.code().unwrap_or(1))
        }
        () = shutdown_signal() => {
            tracing::info!("shutdown signal received, stopping postgres (SIGTERM)");
            if let Some(pid) = child.id() {
                // SIGINT = postgres "fast" shutdown (SIGTERM would be "smart",
                // which waits indefinitely for sessions to end).
                unsafe { libc::kill(pid as i32, libc::SIGINT) };
            }
            let status = child.wait().await.context("wait for postgres shutdown")?;
            tracing::info!(%status, "postgres stopped");
            Ok(0)
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl-c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        _ = terminate => {},
    }
}
