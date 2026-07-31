//! Pure rendering of desired Kubernetes objects from a ClusterSpec.
//! No client here — the reconciler applies what these functions return.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{
    StatefulSet, StatefulSetPersistentVolumeClaimRetentionPolicy, StatefulSetSpec,
};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, EnvVarSource, HTTPGetAction, ObjectFieldSelector,
    PersistentVolumeClaim, PersistentVolumeClaimSpec, PodSpec, PodTemplateSpec, Probe,
    SecretKeySelector, Service, ServiceAccount, ServicePort, ServiceSpec, VolumeMount,
    VolumeResourceRequirements,
};
use k8s_openapi::api::rbac::v1::{PolicyRule, Role, RoleBinding, RoleRef, Subject};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::spec::ClusterSpec;

/// Identity of the spec ConfigMap that owns every rendered object; Kubernetes
/// garbage-collects the children when it is deleted.
#[derive(Debug, Clone)]
pub struct OwnerRef {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub uid: String,
}

pub const AGENT_HTTP_PORT: i32 = 8008;
pub const PG_PORT: i32 = 5432;

pub fn labels(name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "app.kubernetes.io/managed-by".to_string(),
            "pglord".to_string(),
        ),
        ("app.kubernetes.io/instance".to_string(), name.to_string()),
    ])
}

fn meta(name: &str, ns: &str, cluster: &str, owner: &OwnerRef) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        namespace: Some(ns.to_string()),
        labels: Some(labels(cluster)),
        owner_references: Some(vec![OwnerReference {
            api_version: owner.api_version.clone(),
            kind: owner.kind.clone(),
            name: owner.name.clone(),
            uid: owner.uid.clone(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }]),
        ..Default::default()
    }
}

fn http_probe(path: &str) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.to_string()),
            port: IntOrString::Int(AGENT_HTTP_PORT),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn headless_service_name(cluster: &str) -> String {
    format!("{cluster}-pods")
}

pub fn credentials_secret_name(cluster: &str) -> String {
    format!("{cluster}-credentials")
}

pub const SUPERUSER_PASSWORD_KEY: &str = "superuser-password";
pub const REPLICATION_PASSWORD_KEY: &str = "replication-password";

fn secret_env(name: &str, cluster: &str, key: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: credentials_secret_name(cluster),
                key: key.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn pod_fqdn(pod: &str, cluster: &str, ns: &str) -> String {
    format!(
        "{pod}.{}.{ns}.svc.cluster.local",
        headless_service_name(cluster)
    )
}

pub fn instance_sa_name(cluster: &str) -> String {
    format!("{cluster}-instance")
}

/// ServiceAccount + Role + RoleBinding letting instance agents read the spec
/// ConfigMap (primary record) — nothing else.
pub fn render_instance_rbac(
    spec: &ClusterSpec,
    ns: &str,
    owner: &OwnerRef,
) -> (ServiceAccount, Role, RoleBinding) {
    let name = instance_sa_name(&spec.name);
    let sa = ServiceAccount {
        metadata: meta(&name, ns, &spec.name, owner),
        ..Default::default()
    };
    let role = Role {
        metadata: meta(&name, ns, &spec.name, owner),
        // Least privilege: agents may read exactly their own spec ConfigMap
        // (primary record), nothing else in the namespace.
        rules: Some(vec![PolicyRule {
            api_groups: Some(vec![String::new()]),
            resources: Some(vec!["configmaps".to_string()]),
            resource_names: Some(vec![format!("{}-spec", spec.name)]),
            verbs: vec!["get".to_string()],
            ..Default::default()
        }]),
    };
    let binding = RoleBinding {
        metadata: meta(&name, ns, &spec.name, owner),
        role_ref: RoleRef {
            api_group: Some("rbac.authorization.k8s.io".to_string()),
            kind: "Role".to_string(),
            name: name.clone(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: name.clone(),
            namespace: Some(ns.to_string()),
            ..Default::default()
        }]),
    };
    (sa, role, binding)
}

pub fn render_statefulset(spec: &ClusterSpec, ns: &str, owner: &OwnerRef) -> StatefulSet {
    let container = Container {
        name: "postgres".to_string(),
        image: Some(spec.image.clone()),
        image_pull_policy: Some("IfNotPresent".to_string()),
        env: Some(vec![
            EnvVar {
                name: "PGLORD_AGENT_ROLE".to_string(),
                value: Some("auto".to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "PGLORD_AGENT_PRIMARY_HOST".to_string(),
                value: Some(pod_fqdn(&format!("{}-0", spec.name), &spec.name, ns)),
                ..Default::default()
            },
            EnvVar {
                name: "PGLORD_AGENT_CLUSTER".to_string(),
                value: Some(spec.name.clone()),
                ..Default::default()
            },
            EnvVar {
                name: "PGLORD_AGENT_NAMESPACE".to_string(),
                value_from: Some(EnvVarSource {
                    field_ref: Some(ObjectFieldSelector {
                        field_path: "metadata.namespace".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            EnvVar {
                name: "RUST_LOG".to_string(),
                value: Some("info".to_string()),
                ..Default::default()
            },
            secret_env(
                "PGLORD_AGENT_SUPERUSER_PASSWORD",
                &spec.name,
                SUPERUSER_PASSWORD_KEY,
            ),
            secret_env(
                "PGLORD_AGENT_REPL_PASSWORD",
                &spec.name,
                REPLICATION_PASSWORD_KEY,
            ),
        ]),
        ports: Some(vec![
            ContainerPort {
                container_port: PG_PORT,
                name: Some("postgres".to_string()),
                ..Default::default()
            },
            ContainerPort {
                container_port: AGENT_HTTP_PORT,
                name: Some("agent".to_string()),
                ..Default::default()
            },
        ]),
        readiness_probe: Some(http_probe("/readyz")),
        liveness_probe: Some(http_probe("/healthz")),
        volume_mounts: Some(vec![VolumeMount {
            name: "data".to_string(),
            mount_path: "/var/lib/postgresql/data".to_string(),
            ..Default::default()
        }]),
        ..Default::default()
    };

    StatefulSet {
        metadata: meta(&spec.name, ns, &spec.name, owner),
        spec: Some(StatefulSetSpec {
            replicas: Some(i32::from(spec.replicas)),
            service_name: Some(headless_service_name(&spec.name)),
            // PVCs created from volumeClaimTemplates carry no ownerReferences,
            // so without this they outlive the cluster as orphaned storage.
            persistent_volume_claim_retention_policy: Some(
                StatefulSetPersistentVolumeClaimRetentionPolicy {
                    when_deleted: Some("Delete".to_string()),
                    when_scaled: Some("Delete".to_string()),
                },
            ),
            selector: LabelSelector {
                match_labels: Some(labels(&spec.name)),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels(&spec.name)),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    service_account_name: Some(instance_sa_name(&spec.name)),
                    containers: vec![container],
                    ..Default::default()
                }),
            },
            volume_claim_templates: Some(vec![PersistentVolumeClaim {
                metadata: ObjectMeta {
                    name: Some("data".to_string()),
                    ..Default::default()
                },
                spec: Some(PersistentVolumeClaimSpec {
                    access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                    resources: Some(VolumeResourceRequirements {
                        requests: Some(BTreeMap::from([(
                            "storage".to_string(),
                            Quantity(spec.storage_size.clone()),
                        )])),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn render_headless_service(spec: &ClusterSpec, ns: &str, owner: &OwnerRef) -> Service {
    Service {
        metadata: meta(&headless_service_name(&spec.name), ns, &spec.name, owner),
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".to_string()),
            selector: Some(labels(&spec.name)),
            ports: Some(vec![ServicePort {
                name: Some("postgres".to_string()),
                port: PG_PORT,
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Read-write service, pinned to the *recorded* primary pod via the
/// StatefulSet pod-name label. Failover repoints it by re-rendering with the
/// new record.
pub fn render_rw_service(
    spec: &ClusterSpec,
    ns: &str,
    owner: &OwnerRef,
    primary_pod: &str,
) -> Service {
    let mut selector = labels(&spec.name);
    selector.insert(
        "statefulset.kubernetes.io/pod-name".to_string(),
        primary_pod.to_string(),
    );
    Service {
        metadata: meta(&format!("{}-rw", spec.name), ns, &spec.name, owner),
        spec: Some(ServiceSpec {
            selector: Some(selector),
            ports: Some(vec![ServicePort {
                name: Some("postgres".to_string()),
                port: PG_PORT,
                target_port: Some(IntOrString::Int(PG_PORT)),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> (ClusterSpec, OwnerRef) {
        let spec: ClusterSpec =
            serde_json::from_value(serde_json::json!({ "name": "demo", "replicas": 2 }))
                .expect("spec");
        let owner = OwnerRef {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            name: "demo-spec".into(),
            uid: "uid-123".into(),
        };
        (spec, owner)
    }

    #[test]
    fn statefulset_topology_is_correct() {
        let (spec, owner) = demo();
        let sts = render_statefulset(&spec, "pglord", &owner);
        let s = sts.spec.expect("spec");
        assert_eq!(s.replicas, Some(2));
        assert_eq!(s.service_name.as_deref(), Some("demo-pods"));
        let pod = s.template.spec.expect("pod spec");
        let c = &pod.containers[0];
        let env = c.env.as_ref().expect("env");
        assert!(
            env.iter()
                .any(|e| e.name == "PGLORD_AGENT_ROLE" && e.value.as_deref() == Some("auto"))
        );
        assert!(env.iter().any(|e| e.name == "PGLORD_AGENT_PRIMARY_HOST"
            && e.value.as_deref() == Some("demo-0.demo-pods.pglord.svc.cluster.local")));
        let ready = c.readiness_probe.as_ref().expect("readiness");
        assert_eq!(
            ready.http_get.as_ref().expect("http").path.as_deref(),
            Some("/readyz")
        );
        let pvc = &s.volume_claim_templates.expect("pvc")[0];
        let req = pvc
            .spec
            .as_ref()
            .expect("pvc spec")
            .resources
            .as_ref()
            .expect("resources")
            .requests
            .as_ref()
            .expect("requests")
            .clone();
        assert_eq!(req["storage"].0, "1Gi");
    }

    #[test]
    fn volumes_are_deleted_with_the_cluster() {
        // Without this policy, deleting a cluster silently leaves its data
        // volumes (and their cost) behind.
        let (spec, owner) = demo();
        let policy = render_statefulset(&spec, "ns", &owner)
            .spec
            .expect("spec")
            .persistent_volume_claim_retention_policy
            .expect("retention policy");
        assert_eq!(policy.when_deleted.as_deref(), Some("Delete"));
        assert_eq!(policy.when_scaled.as_deref(), Some("Delete"));
    }

    #[test]
    fn statefulset_wires_credentials_from_secret() {
        let (spec, owner) = demo();
        let sts = render_statefulset(&spec, "pglord", &owner);
        let pod = sts.spec.expect("spec").template.spec.expect("pod");
        let env = pod.containers[0].env.as_ref().expect("env");
        for (var, key) in [
            ("PGLORD_AGENT_SUPERUSER_PASSWORD", SUPERUSER_PASSWORD_KEY),
            ("PGLORD_AGENT_REPL_PASSWORD", REPLICATION_PASSWORD_KEY),
        ] {
            let e = env.iter().find(|e| e.name == var).expect(var);
            let sel = e
                .value_from
                .as_ref()
                .expect("value_from")
                .secret_key_ref
                .as_ref()
                .expect("secret ref");
            assert_eq!(sel.name, "demo-credentials");
            assert_eq!(sel.key, key);
        }
    }

    #[test]
    fn instance_role_is_least_privilege() {
        let (spec, owner) = demo();
        let (_sa, role, _binding) = render_instance_rbac(&spec, "ns", &owner);
        let rule = &role.rules.expect("rules")[0];
        assert_eq!(rule.verbs, vec!["get"]);
        assert_eq!(
            rule.resource_names.as_ref().expect("resource names"),
            &vec!["demo-spec".to_string()]
        );
    }

    #[test]
    fn services_select_correctly() {
        let (spec, owner) = demo();
        let headless = render_headless_service(&spec, "pglord", &owner);
        let hs = headless.spec.expect("spec");
        assert_eq!(hs.cluster_ip.as_deref(), Some("None"));
        assert_eq!(
            hs.selector.expect("selector")["app.kubernetes.io/instance"],
            "demo"
        );

        let rw = render_rw_service(&spec, "pglord", &owner, "demo-1");
        assert_eq!(rw.metadata.name.as_deref(), Some("demo-rw"));
        let sel = rw.spec.expect("spec").selector.expect("selector");
        assert_eq!(sel["statefulset.kubernetes.io/pod-name"], "demo-1");
    }

    #[test]
    fn every_object_carries_owner_and_labels() {
        let (spec, owner) = demo();
        for meta in [
            render_statefulset(&spec, "ns", &owner).metadata,
            render_headless_service(&spec, "ns", &owner).metadata,
            render_rw_service(&spec, "ns", &owner, "demo-0").metadata,
            render_instance_rbac(&spec, "ns", &owner).0.metadata,
        ] {
            let refs = meta.owner_references.expect("owner refs");
            assert_eq!(refs[0].uid, "uid-123");
            assert_eq!(
                meta.labels.expect("labels")["app.kubernetes.io/managed-by"],
                "pglord"
            );
        }
    }
}
