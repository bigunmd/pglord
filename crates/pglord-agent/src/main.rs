//! pglord-agent entrypoint: bootstrap PGDATA, run postgres as a supervised
//! child, serve the internal control endpoint, and follow the primary record.

use std::sync::Arc;

use pglord_agent::config::{Config, Role};
use pglord_agent::{bootstrap, http, record, rolestrategy, supervisor};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut cfg = Config::load()?;
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_string())
        .unwrap_or_default();

    // Role resolution: the primary record wins when reachable; ordinal
    // fallback keeps K8s-less environments (docker e2e) working.
    let reader = record::RecordReader::try_new(&cfg).await;
    cfg.role = match &reader {
        Some(r) => match record::apply_record_to_config(&mut cfg, r, &hostname).await {
            Some(role) => role,
            None => rolestrategy::resolve_role(cfg.role, &hostname)?,
        },
        None => rolestrategy::resolve_role(cfg.role, &hostname)?,
    };
    // Read once, before bootstrap: a cluster created by a restore has to know
    // where its data comes from before it has any, and the init scripts are
    // part of the same spec.
    let cluster_spec = match &reader {
        Some(r) => r.spec().await,
        None => None,
    };
    let restore = cluster_spec.as_ref().and_then(|s| s.restore.clone());
    let restored = restore.is_some();

    let cfg = Arc::new(cfg);
    tracing::info!(role = ?cfg.role, %hostname, restored, version = env!("CARGO_PKG_VERSION"), "pglord-agent starting");

    let hba_rules = cluster_spec
        .as_ref()
        .map(|s| s.pg_hba.clone())
        .unwrap_or_default();
    bootstrap::ensure_bootstrapped(&cfg, restore.as_ref(), &hba_rules).await?;
    // One write covers every bootstrap path (initdb, basebackup, restore,
    // already-bootstrapped); the watch loop converges it from then on.
    let sync_conf = pglord_core::sync::render(
        &cfg.cluster,
        cluster_spec.as_ref().map(|s| s.replicas).unwrap_or(1),
        &hostname,
        cluster_spec
            .as_ref()
            .map(|s| s.synchronous)
            .unwrap_or(false),
    );
    if let Err(err) = pglord_agent::params::write_sync_if_changed(&cfg.pgdata, &sync_conf) {
        tracing::warn!(%err, "failed to write sync replication settings");
    }
    // A rebuilt standby carries the primary's control file, so the clamp is
    // correct exactly when postgres first validates hot-standby settings.
    let empty = std::collections::BTreeMap::new();
    let live = if cfg.role == Role::Standby {
        pglord_agent::controldata::live_values(&cfg.pgdata).await
    } else {
        None
    };
    let standby_conf = pglord_core::standby::render(
        cluster_spec
            .as_ref()
            .map(|s| &s.parameters)
            .unwrap_or(&empty),
        live.as_ref(),
    );
    if let Err(err) = pglord_agent::params::write_standby_if_changed(&cfg.pgdata, &standby_conf) {
        tracing::warn!(%err, "failed to write hot-standby floor settings");
    }
    // fsGroup ownership fixes add group bits to the data directory, and
    // postgres refuses to start with them (it demands 0700/0750). Restore the
    // strict mode after every bootstrap path, right before the check runs.
    if let Err(err) = std::fs::set_permissions(
        &cfg.pgdata,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    ) {
        tracing::warn!(%err, "failed to set PGDATA permissions");
    }
    let child = supervisor::spawn_postgres(&cfg)?;

    if cfg.role == Role::Primary {
        let cfg_pb = cfg.clone();
        let spec_pb = cluster_spec.clone();
        // Restored data carries the SOURCE cluster's roles, so this cluster's
        // own generated password would not open it. Reset the superuser too.
        tokio::spawn(async move {
            pglord_agent::postboot::ensure_roles(&cfg_pb, restored).await;
            // Initialisation follows role setup, and only for a cluster that
            // started empty: restored data is already someone's database.
            if let Some(spec) = spec_pb {
                pglord_agent::postboot::ensure_initialised(&cfg_pb, &spec).await;
            }
        });
    }
    if let Some(reader) = reader {
        tokio::spawn(record::follow(reader, cfg.clone(), hostname.clone()));
    }

    let listener = tokio::net::TcpListener::bind(cfg.http_addr).await?;
    tracing::info!(addr = %cfg.http_addr, "control endpoint listening");
    let http_server = axum::serve(listener, http::router(cfg.clone()));

    tokio::select! {
        code = supervisor::supervise(child) => {
            let code = code?;
            tracing::info!(code, "postgres supervision finished, exiting");
            std::process::exit(code);
        }
        result = http_server.into_future() => {
            result?;
            anyhow::bail!("control endpoint stopped unexpectedly");
        }
    }
}
