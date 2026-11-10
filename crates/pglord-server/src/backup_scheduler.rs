//! Scheduled backups.
//!
//! Every minute, each cluster with a schedule is checked against its cron
//! expression: if the schedule has fired since the last run finished starting,
//! and nothing is in flight, a backup goes out. Deliberately simple — no
//! catch-up storm after downtime, just "one run per fire time, at most".

use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::Api;
use pglord_core::backup;

use crate::app::ClusterOps;
use crate::spec_store::{configmap_to_spec, spec_label_selector};

const TICK: Duration = Duration::from_secs(60);

/// Has the schedule fired since the last run started?
///
/// Pure so the rule is testable: a missed window (server down) triggers one
/// run when it comes back, not one per window it slept through.
pub fn is_due(schedule: &str, last_run_started: Option<i64>, now: DateTime<Utc>) -> bool {
    let Ok(parsed) = backup::parse_schedule(schedule) else {
        return false;
    };
    // The most recent fire time at or before now.
    let Some(previous_fire) = parsed
        .after(&(now - chrono::Duration::days(366)))
        .take_while(|t| *t <= now)
        .last()
    else {
        return false; // has not fired yet
    };
    match last_run_started {
        Some(started) => Utc
            .timestamp_opt(started, 0)
            .single()
            .is_none_or(|last| last < previous_fire),
        None => true,
    }
}

pub async fn run(ops: ClusterOps) {
    let cms: Api<ConfigMap> = if ops.all_namespaces {
        Api::all(ops.kube.clone())
    } else {
        Api::namespaced(ops.kube.clone(), &ops.ns)
    };
    loop {
        tokio::time::sleep(TICK).await;
        let specs = match cms
            .list(&kube::api::ListParams::default().labels(&spec_label_selector()))
            .await
        {
            Ok(list) => list,
            Err(err) => {
                tracing::warn!(%err, "scheduler could not list clusters");
                continue;
            }
        };
        for cm in &specs.items {
            let Ok((spec, _owner)) = configmap_to_spec(cm) else {
                continue;
            };
            // Cluster-wide listing: run history and the backup itself live in
            // the item's own namespace.
            let item_ns = cm
                .metadata
                .namespace
                .clone()
                .unwrap_or_else(|| ops.ns.clone());
            let Some(schedule) = spec
                .backup
                .schedule
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .filter(|_| spec.backup.enabled)
                // Asleep clusters take no backups; is_due's catch-up rule
                // produces exactly one scheduled run after wake.
                .filter(|_| !spec.hibernated)
            else {
                continue;
            };

            // A cluster soaking after a major-upgrade cutover has a fenced old
            // primary and a new one whose stanza still describes the old
            // major; finalize runs stanza-upgrade and backups resume then.
            if matches!(
                crate::spec_store::upgrade_from_cm(cm).map(|u| u.phase),
                Some(pglord_core::upgrade::UpgradePhase::Soaking)
            ) {
                tracing::info!(
                    cluster = %spec.name,
                    "skipping scheduled backup: major upgrade soaking"
                );
                continue;
            }

            let runs = match crate::backup_runs::list(&ops.kube, &item_ns, &spec.name).await {
                Ok(runs) => runs,
                Err(err) => {
                    tracing::warn!(cluster = %spec.name, %err, "could not read backup history");
                    continue;
                }
            };
            if crate::backup_runs::has_run_in_flight(&runs) {
                tracing::debug!(cluster = %spec.name, "skipping scheduled backup: one is running");
                continue;
            }
            let last_started = runs.iter().map(|r| r.started).max();
            if !is_due(schedule.trim(), last_started, Utc::now()) {
                continue;
            }
            tracing::info!(cluster = %spec.name, %schedule, "scheduled backup due");
            if let Err(err) = crate::api::backups::start_backup(
                &ops,
                &item_ns,
                &spec,
                spec.backup.schedule_kind,
                "schedule",
            )
            .await
            {
                tracing::warn!(cluster = %spec.name, %err, "scheduled backup could not start");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("timestamp")
            .with_timezone(&Utc)
    }

    #[test]
    fn due_when_nothing_has_run_yet() {
        // Daily at 03:00; it is 04:00, so 03:00 has fired and nothing ran.
        assert!(is_due("0 3 * * *", None, at("2026-08-01T04:00:00Z")));
    }

    #[test]
    fn not_due_again_after_a_run_in_this_window() {
        let last = at("2026-08-01T03:00:30Z").timestamp();
        assert!(!is_due("0 3 * * *", Some(last), at("2026-08-01T04:00:00Z")));
        // Still not due later the same day.
        assert!(!is_due("0 3 * * *", Some(last), at("2026-08-01T23:59:00Z")));
    }

    #[test]
    fn due_again_at_the_next_fire_time() {
        let last = at("2026-08-01T03:00:30Z").timestamp();
        assert!(is_due("0 3 * * *", Some(last), at("2026-08-02T03:00:05Z")));
    }

    #[test]
    fn not_due_before_the_first_fire_of_the_day() {
        let last = at("2026-07-31T03:00:30Z").timestamp();
        // 02:00 on the 1st: the 03:00 window has not arrived yet, and the
        // previous fire (yesterday 03:00) already has a run.
        assert!(!is_due("0 3 * * *", Some(last), at("2026-08-01T02:00:00Z")));
    }

    #[test]
    fn downtime_produces_one_catch_up_run_not_many() {
        // Hourly schedule, last run three days ago: exactly one run is due now,
        // and once it happens the next check is quiet until the next hour.
        let long_ago = at("2026-07-29T03:00:00Z").timestamp();
        let now = at("2026-08-01T12:30:00Z");
        assert!(is_due("0 * * * *", Some(long_ago), now));
        let just_ran = at("2026-08-01T12:30:10Z").timestamp();
        assert!(!is_due("0 * * * *", Some(just_ran), now));
    }

    #[test]
    fn an_invalid_schedule_never_fires() {
        assert!(!is_due("not a cron", None, Utc::now()));
    }
}
