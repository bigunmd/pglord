//! Permission preflight. Instead of guessing from Role objects, ask the
//! apiserver directly via SelfSubjectAccessReview: "may I do X here?". Default
//! Kubernetes RBAC lets every authenticated identity create SSARs, so running
//! this check needs no extra permission of its own.

use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
};
use kube::api::{Api, PostParams};
use serde::Serialize;

#[derive(Debug, Clone, Copy)]
pub struct PermissionCheck {
    /// API group; empty string = core.
    pub group: &'static str,
    pub resource: &'static str,
    pub verb: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub resource: String,
    pub verb: String,
    /// None = the check itself could not be performed.
    pub allowed: Option<bool>,
    pub reason: Option<String>,
}

/// Everything pglord needs in its own namespace to manage clusters.
pub fn required(spec_backend: &str) -> Vec<PermissionCheck> {
    const WRITE: [&str; 5] = ["get", "list", "create", "patch", "delete"];
    let mut out = Vec::new();
    for resource in ["configmaps", "secrets", "services", "serviceaccounts"] {
        for verb in WRITE {
            out.push(PermissionCheck {
                group: "",
                resource,
                verb,
            });
        }
    }
    // Pods are observed, never created directly (the StatefulSet owns them).
    for verb in ["get", "list", "watch"] {
        out.push(PermissionCheck {
            group: "",
            resource: "pods",
            verb,
        });
    }
    // Claims are created by the StatefulSet controller, not by pglord — but
    // growing storage patches them and scaling down deletes the orphans, so
    // without these two verbs both day-2 operations fail with a permission
    // error that nothing else would have warned about.
    for verb in ["get", "list", "patch", "delete"] {
        out.push(PermissionCheck {
            group: "",
            resource: "persistentvolumeclaims",
            verb,
        });
    }
    // Deployments carry the managed PgBouncer pooler; delete matters because
    // disabling the pooler removes its objects.
    for resource in ["statefulsets", "deployments"] {
        for verb in WRITE {
            out.push(PermissionCheck {
                group: "apps",
                resource,
                verb,
            });
        }
    }
    // Operator mode: PostgresCluster resources are the spec store, and the
    // sync loop writes their status subresource.
    if spec_backend == "crd" {
        for verb in ["get", "list", "watch", "create", "patch", "delete"] {
            out.push(PermissionCheck {
                group: "pglord.dev",
                resource: "postgresclusters",
                verb,
            });
        }
        for verb in ["get", "patch"] {
            out.push(PermissionCheck {
                group: "pglord.dev",
                resource: "postgresclusters/status",
                verb,
            });
        }
    }
    // Per-cluster instance RBAC lets agents read their own primary record.
    for resource in ["roles", "rolebindings"] {
        for verb in ["get", "create", "patch"] {
            out.push(PermissionCheck {
                group: "rbac.authorization.k8s.io",
                resource,
                verb,
            });
        }
    }
    out
}

/// Run every required check. Individual failures degrade to `allowed: None`
/// with a reason — the preflight endpoint must never 500 on a flaky apiserver.
pub async fn run(
    client: &kube::Client,
    ns: &str,
    spec_backend: &str,
    all_namespaces: bool,
) -> Vec<CheckResult> {
    let api: Api<SelfSubjectAccessReview> = Api::all(client.clone());
    let mut results = Vec::new();
    for check in required(spec_backend) {
        let review = SelfSubjectAccessReview {
            spec: SelfSubjectAccessReviewSpec {
                resource_attributes: Some(ResourceAttributes {
                    // Empty namespace = "in all namespaces": exactly what the
                    // cluster-wide operator needs granted via ClusterRole.
                    namespace: (!all_namespaces).then(|| ns.to_string()),
                    group: Some(check.group.to_string()),
                    resource: Some(check.resource.to_string()),
                    verb: Some(check.verb.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let (allowed, reason) = match api.create(&PostParams::default(), &review).await {
            Ok(resp) => match resp.status {
                Some(s) => (Some(s.allowed), s.reason),
                None => (None, Some("apiserver returned no status".to_string())),
            },
            Err(err) => (None, Some(err.to_string())),
        };
        results.push(CheckResult {
            resource: qualified(check),
            verb: check.verb.to_string(),
            allowed,
            reason,
        });
    }
    results
}

fn qualified(check: PermissionCheck) -> String {
    if check.group.is_empty() {
        check.resource.to_string()
    } else {
        format!("{}.{}", check.resource, check.group)
    }
}

/// (everything granted, number not granted). An unknown result counts as not
/// granted — preflight reports confidence, not optimism.
pub fn summarize(results: &[CheckResult]) -> (bool, usize) {
    let denied = results.iter().filter(|r| r.allowed != Some(true)).count();
    (denied == 0, denied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(resource: &str, verb: &str, allowed: Option<bool>) -> CheckResult {
        CheckResult {
            resource: resource.to_string(),
            verb: verb.to_string(),
            allowed,
            reason: None,
        }
    }

    #[test]
    fn required_checks_cover_managed_resources_without_duplicates() {
        let checks = required("configmap");
        assert!(!checks.is_empty());
        for resource in [
            "configmaps",
            "secrets",
            "services",
            "serviceaccounts",
            "pods",
            "persistentvolumeclaims",
            "statefulsets",
            "deployments",
            "roles",
            "rolebindings",
        ] {
            assert!(
                checks.iter().any(|c| c.resource == resource),
                "missing checks for {resource}"
            );
        }
        // Growing storage patches claims and scaling down deletes them. These
        // were missing from both the Role and this list, so the operations
        // failed with a permission error the Permissions page said nothing
        // about.
        for verb in ["patch", "delete"] {
            assert!(
                checks
                    .iter()
                    .any(|c| c.resource == "persistentvolumeclaims" && c.verb == verb),
                "missing {verb} on persistentvolumeclaims"
            );
        }
        let mut seen: Vec<(&str, &str, &str)> = checks
            .iter()
            .map(|c| (c.group, c.resource, c.verb))
            .collect();
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), total, "duplicate permission checks");
    }

    #[test]
    fn crd_backend_adds_postgrescluster_checks_and_configmap_mode_omits_them() {
        let crd = required("crd");
        assert!(
            crd.iter()
                .any(|c| c.group == "pglord.dev" && c.resource == "postgresclusters")
        );
        assert!(
            crd.iter()
                .any(|c| c.resource == "postgresclusters/status" && c.verb == "patch")
        );
        let standalone = required("configmap");
        assert!(standalone.iter().all(|c| c.group != "pglord.dev"));
    }

    #[test]
    fn summarize_counts_denied_and_unknown_as_missing() {
        let all_ok = vec![
            result("configmaps", "get", Some(true)),
            result("secrets", "create", Some(true)),
        ];
        assert_eq!(summarize(&all_ok), (true, 0));

        let one_denied = vec![
            result("configmaps", "get", Some(true)),
            result("statefulsets", "create", Some(false)),
        ];
        assert_eq!(summarize(&one_denied), (false, 1));

        // Unknown is not a pass: we cannot claim a permission we could not verify.
        let one_unknown = vec![
            result("configmaps", "get", Some(true)),
            result("roles", "patch", None),
        ];
        assert_eq!(summarize(&one_unknown), (false, 1));
    }

    #[test]
    fn qualified_names_include_group_for_non_core() {
        assert_eq!(
            qualified(PermissionCheck {
                group: "",
                resource: "secrets",
                verb: "get"
            }),
            "secrets"
        );
        assert_eq!(
            qualified(PermissionCheck {
                group: "apps",
                resource: "statefulsets",
                verb: "get"
            }),
            "statefulsets.apps"
        );
    }
}
