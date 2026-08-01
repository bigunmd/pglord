//! Failover orchestration. Polls every instance's agent, detects primary
//! failure, elects the most-advanced ready standby, and flips the primary
//! record via CAS. Everything downstream (rw Service, agent fencing,
//! promotion) follows the record — this loop only decides who the record
//! names.

use std::collections::HashMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::Api;
use pglord_core::render::{AGENT_HTTP_PORT, pod_fqdn};

use crate::instance_probe::observe_cluster;
use crate::primary_record::{cas_set_primary, parse_lsn, recorded_primary};
use crate::reconciler;
use crate::spec_store::{configmap_to_spec, spec_label_selector};

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const FAILURES_BEFORE_FAILOVER: u32 = 3;
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// Pure election rule: among ready in-recovery standbys, pick the highest LSN.
pub fn elect_candidate(
    observations: &[(String, bool, Option<bool>, Option<String>)],
) -> Option<String> {
    observations
        .iter()
        .filter(|(_, ready, in_recovery, _)| *ready && *in_recovery == Some(true))
        .filter_map(|(pod, _, _, lsn)| lsn.as_deref().and_then(parse_lsn).map(|l| (pod.clone(), l)))
        .max_by_key(|(_, lsn)| *lsn)
        .map(|(pod, _)| pod)
}

pub async fn run(client: kube::Client, ns: String, metrics: crate::metrics::Metrics) {
    let http = match reqwest::Client::builder().timeout(STATUS_TIMEOUT).build() {
        Ok(c) => c,
        Err(err) => {
            tracing::error!(%err, "orchestrator http client failed to build; disabled");
            return;
        }
    };
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);
    // cluster -> consecutive failed polls of its recorded primary
    let mut failures: HashMap<String, u32> = HashMap::new();

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let specs = match cms
            .list(&kube::api::ListParams::default().labels(&spec_label_selector()))
            .await
        {
            Ok(list) => list,
            Err(err) => {
                tracing::warn!(%err, "orchestrator list failed");
                continue;
            }
        };
        for cm in &specs.items {
            let Ok((spec, owner)) = configmap_to_spec(cm) else {
                continue;
            };
            let primary = recorded_primary(cm, &spec.name);
            let observations = observe_cluster(&http, &spec.name, &ns, spec.replicas).await;
            metrics
                .instances
                .with_label_values(&[spec.name.as_str()])
                .set(i64::from(spec.replicas));
            metrics
                .instances_ready
                .with_label_values(&[spec.name.as_str()])
                .set(observations.iter().filter(|o| o.ready).count() as i64);

            let primary_ok = observations
                .iter()
                .find(|o| o.pod == primary)
                .map(|o| o.ready)
                .unwrap_or(false);

            let count = failures.entry(spec.name.clone()).or_insert(0);
            if primary_ok {
                *count = 0;
                continue;
            }
            *count += 1;
            tracing::warn!(cluster = %spec.name, %primary, failures = *count, "primary unhealthy");
            if *count < FAILURES_BEFORE_FAILOVER {
                continue;
            }

            let flat: Vec<_> = observations
                .iter()
                .map(|o| (o.pod.clone(), o.ready, o.in_recovery, o.lsn.clone()))
                .collect();
            let Some(candidate) = elect_candidate(&flat) else {
                tracing::error!(cluster = %spec.name, "failover needed but no viable candidate");
                continue;
            };

            let rv = cm.metadata.resource_version.clone().unwrap_or_default();
            let cm_name = cm.metadata.name.clone().unwrap_or_default();
            match cas_set_primary(&cms, &cm_name, &rv, &candidate).await {
                Ok(true) => {
                    tracing::warn!(cluster = %spec.name, old = %primary, new = %candidate, "FAILOVER: primary record updated");
                    metrics
                        .failover_total
                        .with_label_values(&[spec.name.as_str(), "auto"])
                        .inc();
                    *count = 0;
                    // Belt and suspenders: the candidate also self-promotes by
                    // following the record; POST is just faster.
                    let url = format!(
                        "http://{}:{}/promote",
                        pod_fqdn(&candidate, &spec.name, &ns),
                        AGENT_HTTP_PORT
                    );
                    if let Err(err) = http.post(&url).send().await {
                        tracing::debug!(%err, "promote POST failed (agent will self-promote)");
                    }
                    // Repoint -rw immediately instead of waiting for resync.
                    if let Err(err) =
                        reconciler::apply_cluster(&client, &ns, &spec, &owner, &candidate).await
                    {
                        tracing::warn!(%err, "post-failover apply failed (resync will retry)");
                    }
                }
                Ok(false) => {
                    tracing::info!(cluster = %spec.name, "CAS lost; another writer updated the record");
                    *count = 0;
                }
                Err(err) => tracing::warn!(%err, "CAS failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elects_highest_lsn_ready_standby() {
        let obs = vec![
            ("demo-0".into(), false, None, None), // dead primary
            ("demo-1".into(), true, Some(true), Some("0/5000000".into())),
            ("demo-2".into(), true, Some(true), Some("0/5000A00".into())),
        ];
        assert_eq!(elect_candidate(&obs).as_deref(), Some("demo-2"));
    }

    #[test]
    fn single_candidate_wins() {
        let obs = vec![
            ("demo-0".into(), false, None, None),
            ("demo-1".into(), true, Some(true), Some("0/1".into())),
        ];
        assert_eq!(elect_candidate(&obs).as_deref(), Some("demo-1"));
    }

    #[test]
    fn candidate_without_lsn_is_not_eligible() {
        // A standby that cannot report its position must never be promoted:
        // we would have no idea how much WAL it is missing.
        let obs = vec![
            ("demo-1".into(), true, Some(true), None),
            ("demo-2".into(), true, Some(true), Some("0/2".into())),
        ];
        assert_eq!(elect_candidate(&obs).as_deref(), Some("demo-2"));

        let none_reporting = vec![("demo-1".into(), true, Some(true), None)];
        assert_eq!(elect_candidate(&none_reporting), None);
    }

    #[test]
    fn equal_lsns_pick_deterministically() {
        let obs = vec![
            ("demo-1".into(), true, Some(true), Some("0/5000000".into())),
            ("demo-2".into(), true, Some(true), Some("0/5000000".into())),
        ];
        // max_by_key keeps the last maximum; the rule just has to be stable so
        // two orchestrator passes over the same data agree.
        let first = elect_candidate(&obs);
        assert_eq!(first, elect_candidate(&obs));
        assert!(first.is_some());
    }

    #[test]
    fn empty_or_all_unready_yields_no_candidate() {
        assert_eq!(elect_candidate(&[]), None);
        let all_down = vec![
            ("demo-0".into(), false, None, None),
            ("demo-1".into(), false, None, None),
        ];
        assert_eq!(elect_candidate(&all_down), None);
    }

    #[test]
    fn ignores_unready_and_non_recovering_pods() {
        let obs = vec![
            ("demo-1".into(), false, Some(true), Some("0/9000000".into())), // unready
            ("demo-2".into(), true, Some(false), Some("0/8000000".into())), // not standby
        ];
        assert_eq!(elect_candidate(&obs), None);
    }
}
