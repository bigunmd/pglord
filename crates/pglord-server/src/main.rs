//! Process lifecycle only. All routing/wiring lives in app::build.

use std::sync::Arc;

use pglord_server::app::{self, ClusterOps};
use pglord_server::config::Config;
use pglord_server::health::Readiness;
use pglord_server::spec_store::ConfigMapSpecStore;
use pglord_server::{kube_client, reconciler};
use tokio::sync::Notify;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load()?;
    if cfg.all_namespaces && cfg.spec_backend != "crd" {
        anyhow::bail!(
            "PGLORD_ALL_NAMESPACES=true requires PGLORD_SPEC_BACKEND=crd; \
             the configmap backend is single-namespace by design"
        );
    }

    // Early exit: emit the OpenAPI artifact without touching the network.
    if std::env::args().any(|a| a == "--dump-openapi") {
        let (_router, api) = app::build(Readiness::default(), &cfg, None, Default::default());
        std::fs::create_dir_all("spec")?;
        std::fs::write("spec/openapi.yaml", api.to_yaml()?)?;
        println!("wrote spec/openapi.yaml");
        return Ok(());
    }

    // Early exit: emit the CRD manifest without touching the network.
    if std::env::args().any(|a| a == "--dump-crd") {
        use kube::CustomResourceExt;
        std::fs::create_dir_all("deploy/crds")?;
        let yaml = serde_yaml::to_string(&pglord_core::crd::PostgresCluster::crd())?;
        std::fs::write("deploy/crds/postgresclusters.pglord.dev.yaml", yaml)?;
        println!("wrote deploy/crds/postgresclusters.pglord.dev.yaml");
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(&cfg.log_level)
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if !cfg.auth_enabled() {
        tracing::warn!(
            "API authentication is DISABLED — anyone who can reach the server has full \
             control. Set PGLORD_AUTH_TOKENS or PGLORD_AUTH_OIDC_ISSUER to enable it."
        );
    }

    let readiness = Readiness::default();
    let metrics = pglord_server::metrics::Metrics::new();

    let cluster_ops = match kube_client::init(&readiness).await {
        Some(client) => {
            let ns = kube_client::current_namespace();
            tracing::info!(%ns, "kubernetes environment detected");
            tokio::spawn(kube_client::health_loop(client.clone(), readiness.clone()));
            tokio::spawn(reconciler::run(
                client.clone(),
                ns.clone(),
                cfg.all_namespaces,
                metrics.clone(),
            ));
            if cfg.spec_backend == "crd" {
                tokio::spawn(pglord_server::cr_sync::run(
                    client.clone(),
                    ns.clone(),
                    cfg.all_namespaces,
                ));
            }
            tokio::spawn(pglord_server::orchestrator::run(
                client.clone(),
                ns.clone(),
                cfg.all_namespaces,
                metrics.clone(),
            ));

            // One-shot permission report at startup; the full detail is served
            // at /api/v1/preflight and shown in the UI.
            let preflight_client = client.clone();
            let preflight_ns = ns.clone();
            let preflight_backend = cfg.spec_backend.clone();
            let preflight_all_ns = cfg.all_namespaces;
            let preflight_auth_k8s = cfg.auth_kubernetes;
            tokio::spawn(async move {
                let results = pglord_server::preflight::run(
                    &preflight_client,
                    &preflight_ns,
                    &preflight_backend,
                    preflight_all_ns,
                    preflight_auth_k8s,
                )
                .await;
                let (ok, missing) = pglord_server::preflight::summarize(&results);
                let total = results.len();
                if ok {
                    tracing::info!(total, "preflight: all cluster permissions granted");
                } else {
                    let denied: Vec<String> = results
                        .iter()
                        .filter(|r| r.allowed != Some(true))
                        .map(|r| format!("{} {}", r.verb, r.resource))
                        .collect();
                    tracing::warn!(
                        missing,
                        total,
                        denied = %denied.join(", "),
                        "preflight: missing cluster permissions; see /api/v1/preflight"
                    );
                }
            });
            let store: std::sync::Arc<dyn pglord_server::spec_store::SpecStore> =
                match cfg.spec_backend.as_str() {
                    "crd" => std::sync::Arc::new(pglord_server::crd_store::CrdSpecStore::new(
                        client.clone(),
                        &ns,
                        cfg.all_namespaces,
                    )),
                    _ => std::sync::Arc::new(ConfigMapSpecStore::new(client.clone(), &ns)),
                };
            let ops = ClusterOps {
                store,
                kube: client,
                http: reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(2))
                    .build()?,
                ns,
                default_agent_image: cfg.default_agent_image.clone(),
                spec_backend: cfg.spec_backend.clone(),
                all_namespaces: cfg.all_namespaces,
                auth_kubernetes: cfg.auth_kubernetes,
                metrics: metrics.clone(),
            };
            tokio::spawn(pglord_server::backup_scheduler::run(ops.clone()));
            Some(ops)
        }
        None => None,
    };

    let (router, _api) = app::build(readiness, &cfg, cluster_ops, metrics);
    let router = router.layer(
        tower_http::trace::TraceLayer::new_for_http()
            .make_span_with(tower_http::trace::DefaultMakeSpan::new().level(tracing::Level::INFO)),
    );

    let listener = tokio::net::TcpListener::bind(cfg.http_addr).await?;
    tracing::info!(addr = %cfg.http_addr, version = pglord_core::VERSION, "pglord-server listening");

    // Bounded drain: the shutdown timeout must cover only the post-signal drain,
    // not the serve loop itself, so the server is spawned and the signal relayed
    // through a Notify before the timeout starts.
    let signalled = Arc::new(Notify::new());
    let signalled_for_axum = signalled.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                shutdown_signal().await;
                signalled_for_axum.notify_one();
            })
            .await
    });

    signalled.notified().await;
    match tokio::time::timeout(cfg.shutdown_timeout(), server).await {
        Ok(join) => {
            join??;
            tracing::info!("drained cleanly");
        }
        Err(_) => tracing::warn!("drain exceeded shutdown timeout, forcing exit"),
    }
    Ok(())
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
    tracing::info!("shutdown signal received");
}
