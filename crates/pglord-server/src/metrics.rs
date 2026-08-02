//! Control-plane metrics: one long-lived registry with typed handles, cloned
//! into the background loops. Per-cluster series are removed on cluster
//! deletion via forget_cluster so deleted clusters do not linger.

use prometheus::{Encoder, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder};

#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    pub reconcile_total: IntCounterVec,
    pub failover_total: IntCounterVec,
    pub backup_runs_total: IntCounterVec,
    pub rotations_total: IntCounterVec,
    pub instances_ready: IntGaugeVec,
    pub instances: IntGaugeVec,
    pub sync_degraded: IntGaugeVec,
    pub clusters: IntGauge,
}

fn counter_vec(reg: &Registry, name: &str, help: &str, labels: &[&str]) -> IntCounterVec {
    let v = IntCounterVec::new(Opts::new(name, help), labels).expect("static metric name");
    reg.register(Box::new(v.clone()))
        .expect("unique metric name");
    v
}

fn gauge_vec(reg: &Registry, name: &str, help: &str) -> IntGaugeVec {
    let v = IntGaugeVec::new(Opts::new(name, help), &["cluster"]).expect("static metric name");
    reg.register(Box::new(v.clone()))
        .expect("unique metric name");
    v
}

impl Metrics {
    pub fn new() -> Metrics {
        let registry = Registry::new();
        let reconcile_total = counter_vec(
            &registry,
            "pglord_reconcile_total",
            "Reconcile passes per cluster by outcome.",
            &["cluster", "outcome"],
        );
        let failover_total = counter_vec(
            &registry,
            "pglord_failover_total",
            "Primary record changes by trigger (auto = orchestrator, manual = switchover API).",
            &["cluster", "trigger"],
        );
        let backup_runs_total = counter_vec(
            &registry,
            "pglord_backup_runs_total",
            "Finished backup runs by kind and outcome.",
            &["cluster", "kind", "outcome"],
        );
        let rotations_total = counter_vec(
            &registry,
            "pglord_credential_rotations_total",
            "Credential rotations by outcome.",
            &["cluster", "outcome"],
        );
        let instances_ready = gauge_vec(
            &registry,
            "pglord_cluster_instances_ready",
            "Instances currently reporting ready.",
        );
        let instances = gauge_vec(
            &registry,
            "pglord_cluster_instances",
            "Instances the cluster should have.",
        );
        let sync_degraded = gauge_vec(
            &registry,
            "pglord_sync_degraded",
            "1 when synchronous replication is on but no sync standby is connected (writes blocking).",
        );
        let clusters = IntGauge::new("pglord_clusters", "Number of cluster specs.")
            .expect("static metric name");
        registry
            .register(Box::new(clusters.clone()))
            .expect("unique metric name");
        Metrics {
            registry,
            reconcile_total,
            failover_total,
            backup_runs_total,
            rotations_total,
            instances_ready,
            instances,
            sync_degraded,
            clusters,
        }
    }

    pub fn render(&self) -> String {
        let mut buf = Vec::new();
        if TextEncoder::new()
            .encode(&self.registry.gather(), &mut buf)
            .is_err()
        {
            return String::new();
        }
        String::from_utf8(buf).unwrap_or_default()
    }

    /// Drop every series labeled with a deleted cluster. Counter label sets
    /// are finite and known, so removal is by enumeration.
    pub fn forget_cluster(&self, cluster: &str) {
        let _ = self.instances_ready.remove_label_values(&[cluster]);
        let _ = self.instances.remove_label_values(&[cluster]);
        let _ = self.sync_degraded.remove_label_values(&[cluster]);
        for outcome in ["ok", "error"] {
            let _ = self
                .reconcile_total
                .remove_label_values(&[cluster, outcome]);
        }
        for trigger in ["auto", "manual"] {
            let _ = self.failover_total.remove_label_values(&[cluster, trigger]);
        }
        for outcome in ["succeeded", "failed"] {
            let _ = self
                .rotations_total
                .remove_label_values(&[cluster, outcome]);
        }
        for kind in ["full", "diff", "incr"] {
            for outcome in ["succeeded", "failed"] {
                let _ = self
                    .backup_runs_total
                    .remove_label_values(&[cluster, kind, outcome]);
            }
        }
    }
}

impl Default for Metrics {
    fn default() -> Metrics {
        Metrics::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_series_appear_in_render() {
        let m = Metrics::new();
        m.clusters.set(2);
        m.reconcile_total.with_label_values(&["demo", "ok"]).inc();
        m.failover_total.with_label_values(&["demo", "auto"]).inc();
        m.backup_runs_total
            .with_label_values(&["demo", "full", "succeeded"])
            .inc();
        m.instances_ready.with_label_values(&["demo"]).set(3);
        m.sync_degraded.with_label_values(&["demo"]).set(1);
        m.rotations_total
            .with_label_values(&["demo", "succeeded"])
            .inc();
        let out = m.render();
        assert!(out.contains("pglord_clusters 2"));
        assert!(out.contains("pglord_reconcile_total{cluster=\"demo\",outcome=\"ok\"} 1"));
        assert!(out.contains("pglord_failover_total{cluster=\"demo\",trigger=\"auto\"} 1"));
        assert!(out.contains(
            "pglord_backup_runs_total{cluster=\"demo\",kind=\"full\",outcome=\"succeeded\"} 1"
        ));
        assert!(out.contains("pglord_cluster_instances_ready{cluster=\"demo\"} 3"));
        assert!(out.contains("pglord_sync_degraded{cluster=\"demo\"} 1"));
        assert!(out.contains(
            "pglord_credential_rotations_total{cluster=\"demo\",outcome=\"succeeded\"} 1"
        ));
    }

    #[test]
    fn forgetting_a_cluster_drops_its_series() {
        let m = Metrics::new();
        m.reconcile_total.with_label_values(&["gone", "ok"]).inc();
        m.instances_ready.with_label_values(&["gone"]).set(1);
        m.instances.with_label_values(&["gone"]).set(1);
        m.sync_degraded.with_label_values(&["gone"]).set(1);
        m.forget_cluster("gone");
        let out = m.render();
        assert!(!out.contains("cluster=\"gone\""));
    }

    #[test]
    fn each_handle_is_independent() {
        // Two Metrics must not share a registry (fresh test state).
        let a = Metrics::new();
        let b = Metrics::new();
        a.clusters.set(5);
        assert!(!b.render().contains("pglord_clusters 5"));
    }
}
