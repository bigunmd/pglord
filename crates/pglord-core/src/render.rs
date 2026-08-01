//! Pure rendering of desired Kubernetes objects from a ClusterSpec.
//! No client here — the reconciler applies what these functions return.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{
    StatefulSet, StatefulSetPersistentVolumeClaimRetentionPolicy, StatefulSetSpec,
};
use k8s_openapi::api::core::v1::{
    Affinity, ConfigMapVolumeSource, Container, ContainerPort, EnvVar, EnvVarSource, HTTPGetAction,
    LocalObjectReference, ObjectFieldSelector, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    PodAffinityTerm, PodAntiAffinity, PodSpec, PodTemplateSpec, Probe, ResourceRequirements,
    SecretKeySelector, SecretVolumeSource, Service, ServiceAccount, ServicePort, ServiceSpec,
    Toleration as K8sToleration, Volume, VolumeMount, VolumeResourceRequirements,
    WeightedPodAffinityTerm,
};
use k8s_openapi::api::rbac::v1::{PolicyRule, Role, RoleBinding, RoleRef, Subject};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::spec::ClusterSpec;
use crate::workload::{AntiAffinity, Resources, WorkloadSpec};

/// Spread instances across nodes. Preferred by default so a single-node
/// cluster still schedules; required when the user asks for a hard guarantee.
fn anti_affinity(spec: &ClusterSpec) -> Option<Affinity> {
    let term = PodAffinityTerm {
        label_selector: Some(LabelSelector {
            match_labels: Some(BTreeMap::from([(
                "app.kubernetes.io/instance".to_string(),
                spec.name.clone(),
            )])),
            ..Default::default()
        }),
        topology_key: "kubernetes.io/hostname".to_string(),
        ..Default::default()
    };
    let pod_anti_affinity = match spec.workload.anti_affinity {
        AntiAffinity::Disabled => return None,
        AntiAffinity::Required => PodAntiAffinity {
            required_during_scheduling_ignored_during_execution: Some(vec![term]),
            ..Default::default()
        },
        AntiAffinity::Preferred => PodAntiAffinity {
            preferred_during_scheduling_ignored_during_execution: Some(vec![
                WeightedPodAffinityTerm {
                    weight: 100,
                    pod_affinity_term: term,
                },
            ]),
            ..Default::default()
        },
    };
    Some(Affinity {
        pod_anti_affinity: Some(pod_anti_affinity),
        ..Default::default()
    })
}

fn resource_requirements(r: &Resources) -> Option<ResourceRequirements> {
    if r.is_empty() {
        return None;
    }
    let mut requests = BTreeMap::new();
    let mut limits = BTreeMap::new();
    if let Some(v) = &r.cpu_request {
        requests.insert("cpu".to_string(), Quantity(v.clone()));
    }
    if let Some(v) = &r.memory_request {
        requests.insert("memory".to_string(), Quantity(v.clone()));
    }
    if let Some(v) = &r.cpu_limit {
        limits.insert("cpu".to_string(), Quantity(v.clone()));
    }
    if let Some(v) = &r.memory_limit {
        limits.insert("memory".to_string(), Quantity(v.clone()));
    }
    Some(ResourceRequirements {
        requests: (!requests.is_empty()).then_some(requests),
        limits: (!limits.is_empty()).then_some(limits),
        ..Default::default()
    })
}

/// User labels first, managed labels last: a user label can decorate a pod but
/// never shadow the labels the selector and services match on.
fn pod_labels(spec: &ClusterSpec) -> BTreeMap<String, String> {
    let mut merged = spec.workload.labels.clone();
    merged.extend(labels(&spec.name));
    merged
}

/// Scrape defaults first, then user annotations on top: a user-supplied
/// prometheus.io/* key wins, which is the per-cluster opt-out.
fn pod_annotations(spec: &ClusterSpec) -> BTreeMap<String, String> {
    let mut ann = BTreeMap::from([
        ("prometheus.io/scrape".to_string(), "true".to_string()),
        ("prometheus.io/port".to_string(), AGENT_HTTP_PORT.to_string()),
        ("prometheus.io/path".to_string(), "/metrics".to_string()),
    ]);
    ann.extend(spec.workload.annotations.clone());
    ann
}

fn tolerations(w: &WorkloadSpec) -> Option<Vec<K8sToleration>> {
    if w.tolerations.is_empty() {
        return None;
    }
    Some(
        w.tolerations
            .iter()
            .map(|t| K8sToleration {
                key: Some(t.key.clone()),
                operator: t.operator.clone(),
                value: t.value.clone(),
                effect: t.effect.clone(),
                ..Default::default()
            })
            .collect(),
    )
}

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

/// The Secret this cluster's passwords actually come from: the user's when they
/// supplied one, otherwise the generated one.
pub fn credentials_secret_for(spec: &ClusterSpec) -> String {
    spec.credentials_secret()
        .map(str::to_string)
        .unwrap_or_else(|| credentials_secret_name(&spec.name))
}

pub const SUPERUSER_PASSWORD_KEY: &str = "superuser-password";
pub const REPLICATION_PASSWORD_KEY: &str = "replication-password";

fn secret_env(name: &str, spec: &ClusterSpec, key: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: credentials_secret_for(spec),
                key: key.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Read-only mounts for the init script sources.
///
/// Not marked optional: a missing ConfigMap leaves the pod unstarted with the
/// reason in its events, whereas an optional mount would produce a cluster that
/// came up healthy having silently skipped its own initialisation.
fn init_script_mounts(spec: &ClusterSpec) -> Vec<VolumeMount> {
    let mut mounts = Vec::new();
    if spec.init.config_map().is_some() {
        mounts.push(VolumeMount {
            name: crate::init::SCRIPTS_CONFIG_MAP_VOLUME.to_string(),
            mount_path: crate::init::SCRIPTS_CONFIG_MAP_MOUNT.to_string(),
            read_only: Some(true),
            ..Default::default()
        });
    }
    if spec.init.secret().is_some() {
        mounts.push(VolumeMount {
            name: crate::init::SCRIPTS_SECRET_VOLUME.to_string(),
            mount_path: crate::init::SCRIPTS_SECRET_MOUNT.to_string(),
            read_only: Some(true),
            ..Default::default()
        });
    }
    mounts
}

fn init_script_volumes(spec: &ClusterSpec) -> Vec<Volume> {
    let mut volumes = Vec::new();
    if let Some(name) = spec.init.config_map() {
        volumes.push(Volume {
            name: crate::init::SCRIPTS_CONFIG_MAP_VOLUME.to_string(),
            config_map: Some(ConfigMapVolumeSource {
                name: name.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if let Some(name) = spec.init.secret() {
        volumes.push(Volume {
            name: crate::init::SCRIPTS_SECRET_VOLUME.to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(name.to_string()),
                // 0444, not 0400. Kubernetes owns projected secret files as
                // root, and the container runs as postgres — an owner-only mode
                // would leave the scripts unreadable by the process that has to
                // run them. The protection this source provides is that the SQL
                // never lands in a ConfigMap that any namespace reader can see;
                // inside the pod it is on a tmpfs only this pod can reach.
                default_mode: Some(0o444),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    volumes
}

/// The user's own environment variables.
///
/// Deliberately *not* optional: a missing key leaves the pod unstarted with a
/// clear event, which beats a container that comes up with an init script
/// silently missing the password it was supposed to set.
fn custom_env(spec: &ClusterSpec) -> Vec<EnvVar> {
    spec.env
        .iter()
        .map(|e| EnvVar {
            name: e.name.clone(),
            value: e.value.clone(),
            value_from: e.secret.as_ref().map(|s| EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: s.name.clone(),
                    key: s.key.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        })
        .collect()
}

/// Object-storage credentials from the user's Secret. `optional: true` keeps
/// pods schedulable if the Secret is added after the cluster.
///
/// A cluster created by a restore usually has backups of its own switched off,
/// but still has to read the *source's* repository to bootstrap — so the
/// restore repository's Secret is used when there is no backup repository of
/// our own. Without this a restored cluster comes up with no credentials and
/// fails the restore with an authentication error.
fn backup_env(spec: &ClusterSpec) -> Vec<EnvVar> {
    let own = spec
        .backup
        .s3_credentials_secret
        .as_deref()
        .filter(|_| spec.backup.enabled);
    let from_restore = spec
        .restore
        .as_ref()
        .and_then(|r| r.repository.s3_credentials_secret.as_deref());
    let Some(secret) = own.or(from_restore) else {
        return Vec::new();
    };
    [
        ("PGLORD_AGENT_S3_ACCESS_KEY", crate::backup::ACCESS_KEY_KEY),
        ("PGLORD_AGENT_S3_SECRET_KEY", crate::backup::SECRET_KEY_KEY),
    ]
    .into_iter()
    .map(|(var, key)| EnvVar {
        name: var.to_string(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: secret.to_string(),
                key: key.to_string(),
                optional: Some(true),
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
    .collect()
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
        env: Some(
            vec![
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
                    spec,
                    SUPERUSER_PASSWORD_KEY,
                ),
                secret_env("PGLORD_AGENT_REPL_PASSWORD", spec, REPLICATION_PASSWORD_KEY),
            ]
            .into_iter()
            .chain(backup_env(spec))
            // Last, so that even if the reserved-name check were ever bypassed
            // a user variable could not shadow one the agent depends on.
            .chain(custom_env(spec))
            .collect(),
        ),
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
        resources: resource_requirements(&spec.workload.resources),
        volume_mounts: Some(
            std::iter::once(VolumeMount {
                name: "data".to_string(),
                mount_path: "/var/lib/postgresql/data".to_string(),
                ..Default::default()
            })
            .chain(init_script_mounts(spec))
            .collect(),
        ),
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
                    labels: Some(pod_labels(spec)),
                    annotations: Some(pod_annotations(spec)),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    service_account_name: Some(instance_sa_name(&spec.name)),
                    containers: vec![container],
                    affinity: anti_affinity(spec),
                    node_selector: (!spec.workload.node_selector.is_empty())
                        .then(|| spec.workload.node_selector.clone()),
                    tolerations: tolerations(&spec.workload),
                    priority_class_name: spec.workload.priority_class_name.clone(),
                    image_pull_secrets: (!spec.workload.image_pull_secrets.is_empty()).then(|| {
                        spec.workload
                            .image_pull_secrets
                            .iter()
                            .map(|name| LocalObjectReference { name: name.clone() })
                            .collect()
                    }),
                    volumes: {
                        let volumes = init_script_volumes(spec);
                        (!volumes.is_empty()).then_some(volumes)
                    },
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
                    storage_class_name: spec.workload.storage_class.clone(),
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

    fn pod_of(spec: &ClusterSpec, owner: &OwnerRef) -> PodSpec {
        render_statefulset(spec, "ns", owner)
            .spec
            .expect("spec")
            .template
            .spec
            .expect("pod")
    }

    #[test]
    fn a_supplied_credentials_secret_replaces_the_generated_one() {
        let (mut spec, owner) = demo();
        assert_eq!(credentials_secret_for(&spec), "demo-credentials");

        spec.credentials_secret = Some("my-creds".into());
        assert_eq!(credentials_secret_for(&spec), "my-creds");

        // Both password variables have to follow, or the agent would read the
        // superuser password from one Secret and replication from another.
        let env = pod_of(&spec, &owner).containers[0]
            .env
            .clone()
            .expect("env");
        for var in [
            "PGLORD_AGENT_SUPERUSER_PASSWORD",
            "PGLORD_AGENT_REPL_PASSWORD",
        ] {
            let sel = env
                .iter()
                .find(|e| e.name == var)
                .unwrap_or_else(|| panic!("{var}"))
                .value_from
                .as_ref()
                .expect("value_from")
                .secret_key_ref
                .as_ref()
                .expect("secret ref");
            assert_eq!(
                sel.name, "my-creds",
                "{var} must follow the supplied secret"
            );
        }

        // A blank name is not a name: it must fall back to the generated one
        // rather than render a secretKeyRef to "".
        spec.credentials_secret = Some("   ".into());
        assert_eq!(credentials_secret_for(&spec), "demo-credentials");
    }

    #[test]
    fn custom_environment_variables_reach_the_container() {
        let (mut spec, owner) = demo();
        spec.env = serde_json::from_value(serde_json::json!([
            { "name": "APP_USER", "value": "app" },
            { "name": "APP_PASSWORD", "secret": { "name": "app-creds", "key": "password" } }
        ]))
        .expect("env");

        let env = pod_of(&spec, &owner).containers[0]
            .env
            .clone()
            .expect("env");
        let literal = env.iter().find(|e| e.name == "APP_USER").expect("APP_USER");
        assert_eq!(literal.value.as_deref(), Some("app"));

        let from_secret = env
            .iter()
            .find(|e| e.name == "APP_PASSWORD")
            .expect("APP_PASSWORD");
        let sel = from_secret
            .value_from
            .as_ref()
            .expect("value_from")
            .secret_key_ref
            .as_ref()
            .expect("secret ref");
        assert_eq!(sel.name, "app-creds");
        assert_eq!(sel.key, "password");
        // Not optional: a missing key must stop the pod rather than start an
        // instance whose init script silently has no password.
        assert_eq!(sel.optional, None);

        // Custom variables come last, so they cannot displace the agent's own.
        let agent_at = env
            .iter()
            .position(|e| e.name == "PGLORD_AGENT_SUPERUSER_PASSWORD")
            .expect("agent env");
        let custom_at = env
            .iter()
            .position(|e| e.name == "APP_USER")
            .expect("custom env");
        assert!(agent_at < custom_at);
    }

    #[test]
    fn init_scripts_are_mounted_read_only_from_both_sources() {
        let (mut spec, owner) = demo();
        // Nothing configured: no volumes at all, rather than empty ones.
        assert!(pod_of(&spec, &owner).volumes.is_none());

        spec.init = serde_json::from_value(serde_json::json!({
            "scripts_config_map": "init-sql",
            "scripts_secret": "init-secret"
        }))
        .expect("init");

        let pod = pod_of(&spec, &owner);
        let volumes = pod.volumes.expect("volumes");
        let cm = volumes
            .iter()
            .find(|v| v.name == crate::init::SCRIPTS_CONFIG_MAP_VOLUME)
            .expect("configmap volume");
        assert_eq!(cm.config_map.as_ref().expect("cm source").name, "init-sql");

        let secret = volumes
            .iter()
            .find(|v| v.name == crate::init::SCRIPTS_SECRET_VOLUME)
            .expect("secret volume");
        let source = secret.secret.as_ref().expect("secret source");
        assert_eq!(source.secret_name.as_deref(), Some("init-secret"));
        // Readable by the container's user. Kubernetes owns projected secret
        // files as root, so an owner-only mode would hide the scripts from the
        // postgres process that has to run them.
        assert_eq!(source.default_mode, Some(0o444));

        let mounts = pod.containers[0]
            .volume_mounts
            .clone()
            .expect("volume mounts");
        for (volume, path) in [
            (
                crate::init::SCRIPTS_CONFIG_MAP_VOLUME,
                crate::init::SCRIPTS_CONFIG_MAP_MOUNT,
            ),
            (
                crate::init::SCRIPTS_SECRET_VOLUME,
                crate::init::SCRIPTS_SECRET_MOUNT,
            ),
        ] {
            let mount = mounts
                .iter()
                .find(|m| m.name == volume)
                .unwrap_or_else(|| panic!("{volume} mount"));
            assert_eq!(mount.mount_path, path);
            assert_eq!(mount.read_only, Some(true));
        }
        // The data volume is still there; init mounts are additions.
        assert!(mounts.iter().any(|m| m.name == "data"));
    }

    #[test]
    fn backup_credentials_are_mounted_only_when_enabled() {
        let (mut spec, owner) = demo();
        let env_of = |s: &ClusterSpec| {
            render_statefulset(s, "ns", &owner)
                .spec
                .expect("spec")
                .template
                .spec
                .expect("pod")
                .containers[0]
                .env
                .clone()
                .expect("env")
        };
        assert!(
            !env_of(&spec)
                .iter()
                .any(|e| e.name.starts_with("PGLORD_AGENT_S3")),
            "no S3 env before backups are configured"
        );

        spec.backup = crate::backup::BackupSpec {
            enabled: true,
            s3_endpoint: Some("https://s3.example.com".into()),
            s3_bucket: Some("b".into()),
            s3_credentials_secret: Some("backup-creds".into()),
            ..Default::default()
        };
        let env = env_of(&spec);
        let access = env
            .iter()
            .find(|e| e.name == "PGLORD_AGENT_S3_ACCESS_KEY")
            .expect("access key env");
        let sel = access
            .value_from
            .as_ref()
            .expect("value_from")
            .secret_key_ref
            .as_ref()
            .expect("secret ref");
        assert_eq!(sel.name, "backup-creds");
        assert_eq!(sel.key, "access-key");
        // Optional so a missing Secret does not make pods unschedulable.
        assert_eq!(sel.optional, Some(true));
    }

    #[test]
    fn a_restored_cluster_gets_the_source_repositorys_credentials() {
        // A restored cluster has backups of its own switched off, but still has
        // to read the source's repository to bootstrap. Without these env vars
        // the restore fails with an authentication error and the pod
        // crashloops on an empty data directory.
        let (mut spec, owner) = demo();
        spec.restore = Some(
            serde_json::from_value(serde_json::json!({
                "source_cluster": "src",
                "repository": {
                    "enabled": true,
                    "s3_endpoint": "https://s3.example.com",
                    "s3_bucket": "b",
                    "s3_credentials_secret": "source-creds"
                }
            }))
            .expect("restore spec"),
        );
        assert!(
            !spec.backup.enabled,
            "backups stay off on a restored cluster"
        );

        let env = render_statefulset(&spec, "ns", &owner)
            .spec
            .expect("spec")
            .template
            .spec
            .expect("pod")
            .containers[0]
            .env
            .clone()
            .expect("env");
        let sel = env
            .iter()
            .find(|e| e.name == "PGLORD_AGENT_S3_ACCESS_KEY")
            .expect("access key env")
            .value_from
            .as_ref()
            .expect("value_from")
            .secret_key_ref
            .as_ref()
            .expect("secret ref");
        assert_eq!(sel.name, "source-creds");
    }

    #[test]
    fn instances_spread_across_nodes_by_default() {
        let (spec, owner) = demo();
        let pod = render_statefulset(&spec, "ns", &owner)
            .spec
            .expect("spec")
            .template
            .spec
            .expect("pod");
        let anti = pod
            .affinity
            .expect("affinity")
            .pod_anti_affinity
            .expect("anti-affinity");
        let preferred = anti
            .preferred_during_scheduling_ignored_during_execution
            .expect("preferred rule");
        assert_eq!(
            preferred[0].pod_affinity_term.topology_key,
            "kubernetes.io/hostname"
        );
        // Soft by default: a one-node cluster must still schedule.
        assert!(
            anti.required_during_scheduling_ignored_during_execution
                .is_none()
        );
    }

    #[test]
    fn anti_affinity_can_be_required_or_disabled() {
        let (mut spec, owner) = demo();
        spec.workload.anti_affinity = crate::workload::AntiAffinity::Required;
        let anti = render_statefulset(&spec, "ns", &owner)
            .spec
            .expect("spec")
            .template
            .spec
            .expect("pod")
            .affinity
            .expect("affinity")
            .pod_anti_affinity
            .expect("anti-affinity");
        assert!(
            anti.required_during_scheduling_ignored_during_execution
                .is_some()
        );
        assert!(
            anti.preferred_during_scheduling_ignored_during_execution
                .is_none()
        );

        spec.workload.anti_affinity = crate::workload::AntiAffinity::Disabled;
        assert!(
            render_statefulset(&spec, "ns", &owner)
                .spec
                .expect("spec")
                .template
                .spec
                .expect("pod")
                .affinity
                .is_none()
        );
    }

    #[test]
    fn user_labels_decorate_but_never_shadow_managed_ones() {
        let (mut spec, owner) = demo();
        spec.workload.labels = BTreeMap::from([
            ("team".to_string(), "payments".to_string()),
            // A deliberate collision attempt: managed labels must win.
            (
                "app.kubernetes.io/instance".to_string(),
                "hijacked".to_string(),
            ),
        ]);
        spec.workload.annotations =
            BTreeMap::from([("prometheus.io/scrape".to_string(), "true".to_string())]);
        let sts = render_statefulset(&spec, "ns", &owner).spec.expect("spec");
        let meta = sts.template.metadata.expect("pod metadata");
        let pod_labels = meta.labels.expect("labels");
        assert_eq!(pod_labels["team"], "payments");
        assert_eq!(pod_labels["app.kubernetes.io/instance"], "demo");
        assert_eq!(
            meta.annotations.expect("annotations")["prometheus.io/scrape"],
            "true"
        );
        // The selector is immutable in Kubernetes and must stay exactly ours.
        let selector = sts.selector.match_labels.expect("selector");
        assert_eq!(selector, labels("demo"));
    }

    #[test]
    fn scrape_annotations_are_rendered_by_default() {
        let (spec, owner) = demo();
        let sts = render_statefulset(&spec, "ns", &owner).spec.expect("spec");
        let meta = sts.template.metadata.expect("pod metadata");
        let ann = meta.annotations.expect("annotations");
        assert_eq!(ann["prometheus.io/scrape"], "true");
        assert_eq!(ann["prometheus.io/port"], "8008");
        assert_eq!(ann["prometheus.io/path"], "/metrics");
    }

    #[test]
    fn a_user_annotation_overrides_the_scrape_default() {
        let (mut spec, owner) = demo();
        spec.workload.annotations =
            BTreeMap::from([("prometheus.io/scrape".to_string(), "false".to_string())]);
        let sts = render_statefulset(&spec, "ns", &owner).spec.expect("spec");
        let meta = sts.template.metadata.expect("pod metadata");
        let ann = meta.annotations.expect("annotations");
        assert_eq!(ann["prometheus.io/scrape"], "false");
        assert_eq!(ann["prometheus.io/port"], "8008");
    }

    #[test]
    fn resources_scheduling_and_storage_class_are_applied() {
        let (mut spec, owner) = demo();
        spec.workload.resources = crate::workload::Resources {
            cpu_request: Some("100m".into()),
            memory_request: Some("256Mi".into()),
            memory_limit: Some("1Gi".into()),
            cpu_limit: None,
        };
        spec.workload.storage_class = Some("fast-ssd".into());
        spec.workload.node_selector = BTreeMap::from([("disk".into(), "nvme".into())]);
        spec.workload.priority_class_name = Some("database".into());
        spec.workload.image_pull_secrets = vec!["regcred".into()];
        spec.workload.tolerations = vec![crate::workload::Toleration {
            key: "dedicated".into(),
            operator: Some("Equal".into()),
            value: Some("database".into()),
            effect: Some("NoSchedule".into()),
        }];

        let sts = render_statefulset(&spec, "ns", &owner).spec.expect("spec");
        let pod = sts.template.spec.expect("pod");
        let res = pod.containers[0].resources.as_ref().expect("resources");
        assert_eq!(res.requests.as_ref().expect("requests")["cpu"].0, "100m");
        assert_eq!(
            res.requests.as_ref().expect("requests")["memory"].0,
            "256Mi"
        );
        assert_eq!(res.limits.as_ref().expect("limits")["memory"].0, "1Gi");
        assert!(!res.limits.as_ref().expect("limits").contains_key("cpu"));

        assert_eq!(pod.node_selector.expect("node selector")["disk"], "nvme");
        assert_eq!(pod.priority_class_name.as_deref(), Some("database"));
        assert_eq!(
            pod.image_pull_secrets.expect("pull secrets")[0].name,
            "regcred"
        );
        let tol = pod.tolerations.expect("tolerations");
        assert_eq!(tol[0].key.as_deref(), Some("dedicated"));
        assert_eq!(tol[0].effect.as_deref(), Some("NoSchedule"));

        let pvc = &sts.volume_claim_templates.expect("pvc")[0];
        assert_eq!(
            pvc.spec
                .as_ref()
                .expect("pvc spec")
                .storage_class_name
                .as_deref(),
            Some("fast-ssd")
        );
    }

    #[test]
    fn no_resources_declared_means_no_resources_block() {
        let (spec, owner) = demo();
        let pod = render_statefulset(&spec, "ns", &owner)
            .spec
            .expect("spec")
            .template
            .spec
            .expect("pod");
        assert!(pod.containers[0].resources.is_none());
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
