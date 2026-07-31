//! History of backup attempts.
//!
//! The repository listing says what can be restored; this says what *happened*
//! — including runs that are still going and runs that failed, neither of
//! which leaves anything in the repository. Stored in a per-cluster ConfigMap
//! so history survives a server restart.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, Patch, PatchParams};
use serde::{Deserialize, Serialize};

use crate::spec_store::{FIELD_MANAGER, MANAGED_BY_LABEL};

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

/// Runs stuck in `running` past the timeout are reported as unknown: the
/// process that owned them is gone, so we cannot claim they are still going.
pub fn stale_running_to_unknown(mut runs: Vec<BackupRun>, now: i64) -> Vec<BackupRun> {
    for run in runs.iter_mut() {
        if run.state == RunState::Running && now - run.started > RUNNING_TIMEOUT_SECONDS {
            run.state = RunState::Unknown;
            run.error = Some("the server tracking this backup restarted".to_string());
        }
    }
    runs
}

pub fn has_run_in_flight(runs: &[BackupRun]) -> bool {
    runs.iter().any(|r| r.state == RunState::Running)
}

pub async fn list(client: &kube::Client, ns: &str, cluster: &str) -> Vec<BackupRun> {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let Ok(Some(cm)) = api.get_opt(&runs_cm_name(cluster)).await else {
        return Vec::new();
    };
    cm.data
        .as_ref()
        .and_then(|d| d.get(RUNS_KEY))
        .and_then(|raw| serde_json::from_str::<Vec<BackupRun>>(raw).ok())
        .unwrap_or_default()
}

async fn save(
    client: &kube::Client,
    ns: &str,
    cluster: &str,
    runs: &[BackupRun],
) -> anyhow::Result<()> {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let name = runs_cm_name(cluster);
    let cm = ConfigMap {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(ns.to_string()),
            labels: Some(BTreeMap::from([
                (MANAGED_BY_LABEL.to_string(), "pglord".to_string()),
                (
                    "app.kubernetes.io/instance".to_string(),
                    cluster.to_string(),
                ),
            ])),
            ..Default::default()
        },
        data: Some(BTreeMap::from([(
            RUNS_KEY.to_string(),
            serde_json::to_string(runs)?,
        )])),
        ..Default::default()
    };
    api.patch(
        &name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&cm),
    )
    .await?;
    Ok(())
}

pub async fn record_start(
    client: &kube::Client,
    ns: &str,
    cluster: &str,
    run: BackupRun,
) -> anyhow::Result<()> {
    let runs = push_run(list(client, ns, cluster).await, run);
    save(client, ns, cluster, &runs).await
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
    let runs = update_run(list(client, ns, cluster).await, id, |run| {
        run.state = state;
        run.finished = Some(finished);
        run.error.clone_from(&error);
    });
    save(client, ns, cluster, &runs).await
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
        assert!(has_run_in_flight(&[
            run("a", RunState::Succeeded, 1),
            run("b", RunState::Running, 2)
        ]));
    }
}
