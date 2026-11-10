//! History of backup attempts.
//!
//! The repository listing says what can be restored; this says what *happened*
//! — including runs that are still going and runs that failed, neither of
//! which leaves anything in the repository. Stored in a per-cluster ConfigMap
//! so history survives a server restart.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, PostParams};
use serde::{Deserialize, Serialize};

use crate::spec_store::MANAGED_BY_LABEL;

/// Runs kept per cluster. Enough to see a pattern, bounded so the ConfigMap
/// cannot grow without limit.
pub const MAX_RUNS: usize = 20;
/// A run still marked running after this long lost its server; report it as
/// unknown rather than pretending it is still in progress.
pub const RUNNING_TIMEOUT_SECONDS: i64 = 6 * 60 * 60;
pub const RUNS_KEY: &str = "runs";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Running,
    Succeeded,
    Failed,
    /// The server that started this run went away before it finished.
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BackupRun {
    pub id: String,
    /// "full", "diff" or "incr".
    pub kind: String,
    /// "manual" or "schedule".
    pub trigger: String,
    pub state: RunState,
    /// Unix seconds.
    pub started: i64,
    pub finished: Option<i64>,
    pub error: Option<String>,
}

pub fn runs_cm_name(cluster: &str) -> String {
    format!("{cluster}-backup-runs")
}

/// Newest first, capped. Pure so the trimming rule is testable.
pub fn push_run(mut runs: Vec<BackupRun>, run: BackupRun) -> Vec<BackupRun> {
    runs.retain(|r| r.id != run.id);
    runs.insert(0, run);
    runs.truncate(MAX_RUNS);
    runs
}

/// Replace a run in place (same id), keeping order.
pub fn update_run(
    mut runs: Vec<BackupRun>,
    id: &str,
    apply: impl Fn(&mut BackupRun),
) -> Vec<BackupRun> {
    for run in runs.iter_mut() {
        if run.id == id {
            apply(run);
        }
    }
    runs
}

/// Old running records have an unknown outcome; age does not prove the job stopped.
pub fn stale_running_to_unknown(mut runs: Vec<BackupRun>, now: i64) -> Vec<BackupRun> {
    for run in runs.iter_mut() {
        if run.state == RunState::Running && now - run.started > RUNNING_TIMEOUT_SECONDS {
            run.state = RunState::Unknown;
            run.error = Some(
                "backup outcome is unknown; admission remains blocked until reconciled".to_string(),
            );
        }
    }
    runs
}

pub fn has_run_in_flight(runs: &[BackupRun]) -> bool {
    runs.iter()
        .any(|r| matches!(r.state, RunState::Running | RunState::Unknown))
}

fn decode(cm: &ConfigMap) -> anyhow::Result<Vec<BackupRun>> {
    let raw = cm
        .data
        .as_ref()
        .and_then(|d| d.get(RUNS_KEY))
        .ok_or_else(|| anyhow::anyhow!("backup history has no runs field"))?;
    Ok(serde_json::from_str(raw)?)
}

pub async fn list(
    client: &kube::Client,
    ns: &str,
    cluster: &str,
) -> anyhow::Result<Vec<BackupRun>> {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    match api.get_opt(&runs_cm_name(cluster)).await? {
        Some(cm) => decode(&cm),
        None => Ok(Vec::new()),
    }
}

#[derive(Debug)]
pub struct BackupInProgress;
impl std::fmt::Display for BackupInProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a backup is already running")
    }
}
impl std::error::Error for BackupInProgress {}

fn conflict(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(response) if response.code == 409)
}

/// The create/replace is the admission point shared by manual and scheduled runs.
pub async fn record_start(
    client: &kube::Client,
    ns: &str,
    cluster: &str,
    run: BackupRun,
) -> anyhow::Result<()> {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let name = runs_cm_name(cluster);
    for _ in 0..8 {
        let previous = api.get_opt(&name).await?;
        let runs = match &previous {
            Some(cm) => decode(cm)?,
            None => Vec::new(),
        };
        // Age alone cannot prove the old backup stopped. Fail closed until its outcome is known.
        if has_run_in_flight(&runs) {
            return Err(BackupInProgress.into());
        }
        let existing = previous.is_some();
        let mut cm = if let Some(cm) = previous {
            cm
        } else {
            let owner = api.get(&crate::spec_store::spec_cm_name(cluster)).await?;
            let uid = owner
                .metadata
                .uid
                .ok_or_else(|| anyhow::anyhow!("spec has no UID"))?;
            ConfigMap {
                metadata: ObjectMeta {
                    name: Some(name.clone()),
                    namespace: Some(ns.into()),
                    labels: Some(BTreeMap::from([
                        (MANAGED_BY_LABEL.into(), "pglord".into()),
                        ("app.kubernetes.io/instance".into(), cluster.into()),
                    ])),
                    owner_references: Some(vec![OwnerReference {
                        api_version: "v1".into(),
                        kind: "ConfigMap".into(),
                        name: crate::spec_store::spec_cm_name(cluster),
                        uid,
                        controller: Some(true),
                        block_owner_deletion: Some(true),
                    }]),
                    ..Default::default()
                },
                ..Default::default()
            }
        };
        cm.data.get_or_insert_with(BTreeMap::new).insert(
            RUNS_KEY.into(),
            serde_json::to_string(&push_run(runs, run.clone()))?,
        );
        let result = if existing {
            api.replace(&name, &PostParams::default(), &cm).await
        } else {
            api.create(&PostParams::default(), &cm).await
        };
        match result {
            Ok(_) => return Ok(()),
            Err(err) if conflict(&err) => continue,
            Err(err) => return Err(err.into()),
        }
    }
    anyhow::bail!("backup admission conflicted repeatedly")
}

pub async fn record_finish(
    client: &kube::Client,
    ns: &str,
    cluster: &str,
    id: &str,
    state: RunState,
    finished: i64,
    error: Option<String>,
) -> anyhow::Result<()> {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let name = runs_cm_name(cluster);
    for _ in 0..8 {
        let mut cm = api.get(&name).await?;
        let runs = decode(&cm)?;
        anyhow::ensure!(runs.iter().any(|r| r.id == id), "backup run {id} not found");
        let runs = update_run(runs, id, |run| {
            run.state = state;
            run.finished = Some(finished);
            run.error.clone_from(&error);
        });
        cm.data
            .get_or_insert_with(BTreeMap::new)
            .insert(RUNS_KEY.into(), serde_json::to_string(&runs)?);
        match api.replace(&name, &PostParams::default(), &cm).await {
            Ok(_) => return Ok(()),
            Err(err) if conflict(&err) => continue,
            Err(err) => return Err(err.into()),
        }
    }
    anyhow::bail!("backup completion conflicted repeatedly")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str, state: RunState, started: i64) -> BackupRun {
        BackupRun {
            id: id.to_string(),
            kind: "full".to_string(),
            trigger: "manual".to_string(),
            state,
            started,
            finished: None,
            error: None,
        }
    }

    #[derive(Clone, Default)]
    struct MockHistory {
        cm: std::sync::Arc<tokio::sync::Mutex<Option<ConfigMap>>>,
        fail_reads: std::sync::Arc<std::sync::atomic::AtomicBool>,
        writes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    fn status(code: u16) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
        (
            axum::http::StatusCode::from_u16(code).expect("status"),
            axum::Json(serde_json::json!({
                "apiVersion":"v1", "kind":"Status", "status":"Failure",
                "message":"mock error", "reason":match code { 404 => "NotFound", 409 => "Conflict", _ => "InternalError" }, "code":code
            })),
        )
    }

    async fn mock_history() -> (kube::Client, MockHistory) {
        use axum::{
            Json,
            extract::{Path, State},
            routing::{get, post},
        };
        use std::sync::atomic::Ordering;
        let state = MockHistory::default();
        let app = axum::Router::new()
            .route(
                "/api/v1/namespaces/ns/configmaps/{name}",
                get(
                    |State(s): State<MockHistory>, Path(name): Path<String>| async move {
                        if s.fail_reads.load(Ordering::SeqCst) {
                            return Err(status(500));
                        }
                        if name == "db-spec" {
                            return Ok(Json(
                                serde_json::json!({"apiVersion":"v1", "kind":"ConfigMap",
                            "metadata":{"name":name,"uid":"spec-uid"}}),
                            ));
                        }
                        let cm = s.cm.lock().await;
                        cm.as_ref()
                            .map(|cm| Json(serde_json::to_value(cm).expect("json")))
                            .ok_or_else(|| status(404))
                    },
                )
                .put(
                    |State(s): State<MockHistory>, Json(mut incoming): Json<ConfigMap>| async move {
                        let mut stored = s.cm.lock().await;
                        let Some(old) = stored.as_ref() else {
                            return Err(status(404));
                        };
                        if incoming.metadata.resource_version != old.metadata.resource_version {
                            return Err(status(409));
                        }
                        let rev = old
                            .metadata
                            .resource_version
                            .as_ref()
                            .expect("revision")
                            .parse::<u64>()
                            .expect("number")
                            + 1;
                        incoming.metadata.resource_version = Some(rev.to_string());
                        *stored = Some(incoming.clone());
                        s.writes.fetch_add(1, Ordering::SeqCst);
                        Ok(Json(incoming))
                    },
                ),
            )
            .route(
                "/api/v1/namespaces/ns/configmaps",
                post(
                    |State(s): State<MockHistory>, Json(mut incoming): Json<ConfigMap>| async move {
                        let mut stored = s.cm.lock().await;
                        if stored.is_some() {
                            return Err(status(409));
                        }
                        incoming.metadata.resource_version = Some("1".into());
                        *stored = Some(incoming.clone());
                        s.writes.fetch_add(1, Ordering::SeqCst);
                        Ok(Json(incoming))
                    },
                ),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let cfg = kube::Config::new(format!("http://{addr}").parse().expect("URI"));
        (kube::Client::try_from(cfg).expect("client"), state)
    }

    #[tokio::test]
    async fn concurrent_admission_has_one_winner_and_preserves_history() {
        let (client, _) = mock_history().await;
        let (a, b) = tokio::join!(
            record_start(&client, "ns", "db", run("a", RunState::Running, 1)),
            record_start(&client, "ns", "db", run("b", RunState::Running, 1))
        );
        assert_ne!(a.is_ok(), b.is_ok());
        let loser = if let Err(e) = a {
            e
        } else {
            b.expect_err("loser")
        };
        assert!(loser.is::<BackupInProgress>());
        let runs = list(&client, "ns", "db").await.expect("list");
        assert_eq!(runs.len(), 1);
        record_finish(
            &client,
            "ns",
            "db",
            &runs[0].id,
            RunState::Succeeded,
            2,
            None,
        )
        .await
        .expect("finish");
        record_start(&client, "ns", "db", run("next", RunState::Running, 3))
            .await
            .expect("next");
        // A late duplicate completion must not erase the later admitted run.
        record_finish(
            &client,
            "ns",
            "db",
            &runs[0].id,
            RunState::Succeeded,
            2,
            None,
        )
        .await
        .expect("late finish");
        let runs = list(&client, "ns", "db").await.expect("list");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, "next");
        assert_eq!(runs[0].state, RunState::Running);
    }

    #[tokio::test]
    async fn history_read_failure_never_writes_and_missing_is_distinct() {
        use std::sync::atomic::Ordering;
        let (client, state) = mock_history().await;
        assert!(list(&client, "ns", "db").await.expect("missing").is_empty());
        state.fail_reads.store(true, Ordering::SeqCst);
        assert!(list(&client, "ns", "db").await.is_err());
        assert!(
            record_start(&client, "ns", "db", run("a", RunState::Running, 1))
                .await
                .is_err()
        );
        assert!(
            record_finish(&client, "ns", "db", "a", RunState::Succeeded, 2, None)
                .await
                .is_err()
        );
        assert_eq!(state.writes.load(Ordering::SeqCst), 0);
        assert!(decode(&ConfigMap::default()).is_err());
        assert!(
            decode(&ConfigMap {
                data: Some(BTreeMap::from([(RUNS_KEY.into(), "invalid".into())])),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn history_is_newest_first_and_bounded() {
        let mut runs = Vec::new();
        for i in 0..(MAX_RUNS + 5) {
            runs = push_run(runs, run(&format!("r{i}"), RunState::Succeeded, i as i64));
        }
        assert_eq!(runs.len(), MAX_RUNS);
        // The newest survives, the oldest is dropped.
        assert_eq!(runs[0].id, format!("r{}", MAX_RUNS + 4));
        assert!(!runs.iter().any(|r| r.id == "r0"));
    }

    #[test]
    fn finishing_a_run_updates_it_in_place() {
        let runs = push_run(Vec::new(), run("a", RunState::Running, 100));
        let runs = push_run(runs, run("b", RunState::Running, 200));
        let updated = update_run(runs, "a", |r| {
            r.state = RunState::Succeeded;
            r.finished = Some(300);
        });
        let a = updated.iter().find(|r| r.id == "a").expect("run a");
        assert_eq!(a.state, RunState::Succeeded);
        assert_eq!(a.finished, Some(300));
        // Order and the other run are untouched.
        assert_eq!(updated[0].id, "b");
        assert_eq!(updated[1].state, RunState::Succeeded);
    }

    #[test]
    fn abandoned_runs_are_reported_as_unknown() {
        let now = 1_000_000;
        let runs = vec![
            run("fresh", RunState::Running, now - 60),
            run(
                "stale",
                RunState::Running,
                now - RUNNING_TIMEOUT_SECONDS - 1,
            ),
            run("done", RunState::Succeeded, now - 100_000),
        ];
        let mapped = stale_running_to_unknown(runs, now);
        assert_eq!(
            mapped[0].state,
            RunState::Running,
            "a fresh run is still running"
        );
        assert_eq!(mapped[1].state, RunState::Unknown);
        assert!(mapped[1].error.is_some());
        assert_eq!(
            mapped[2].state,
            RunState::Succeeded,
            "finished runs untouched"
        );
    }

    #[test]
    fn in_flight_detection_guards_concurrent_backups() {
        assert!(!has_run_in_flight(&[run("a", RunState::Succeeded, 1)]));
        assert!(has_run_in_flight(&[run(
            "lost-response",
            RunState::Unknown,
            1
        )]));
        assert!(has_run_in_flight(&[
            run("a", RunState::Succeeded, 1),
            run("b", RunState::Running, 2)
        ]));
    }
}
