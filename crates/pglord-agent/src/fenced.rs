//! Boot-time hold for fenced instances: postgres is never started; the agent
//! serves its control endpoint (readyz 503, status role "fenced") and waits
//! for the spec to unfence this pod, then returns so main can exit and the
//! pod restarts into a normal boot.

use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::record::{RecordReader, is_fenced_in};
use crate::{http, status};

/// Returns when the spec no longer lists this instance as fenced. The caller
/// exits so the pod restarts and the full bootstrap path (demote, rewind,
/// rejoin) runs instead of a duplicated rejoin-in-place.
pub async fn hold(cfg: Arc<Config>, reader: &RecordReader, me: &str) -> anyhow::Result<()> {
    status::set_fenced(true);
    let listener = tokio::net::TcpListener::bind(cfg.http_addr).await?;
    tracing::warn!(addr = %cfg.http_addr, "fenced: postgres held down; control endpoint listening");
    let server = axum::serve(listener, http::router(cfg.clone()));
    tokio::select! {
        result = server.into_future() => {
            result?;
            anyhow::bail!("control endpoint stopped while fenced");
        }
        () = wait_unfenced(reader, me) => Ok(()),
    }
}

async fn wait_unfenced(reader: &RecordReader, me: &str) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        match reader.spec().await {
            Some(spec) if !is_fenced_in(&spec.fenced, me) => return,
            Some(_) => {}
            // Unreadable spec: stay fenced — never start postgres on doubt.
            None => tracing::debug!("spec unreadable; staying fenced"),
        }
    }
}
