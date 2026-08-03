//! What a cluster needs to exist in the namespace before it can run.
//!
//! Everything here is derived from the spec, so the list is exact rather than
//! documentation that drifts. The server pairs each entry with whether the
//! object is actually present, which turns "here is what a cluster needs" into
//! "here is what is missing".

use serde::{Deserialize, Serialize};

use crate::backup::{ACCESS_KEY_KEY, SECRET_KEY_KEY};
use crate::render::{
    POOLER_PASSWORD_KEY, REPLICATION_PASSWORD_KEY, SUPERUSER_PASSWORD_KEY, credentials_secret_name,
};
use crate::spec::ClusterSpec;

#[cfg_attr(feature = "crd", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResourceKind {
    Secret,
    ConfigMap,
}

/// Why a cluster needs this object. The UI translates these; keeping them
/// machine-readable means the reason survives translation.
#[cfg_attr(feature = "crd", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePurpose {
    /// Superuser and replication passwords.
    Credentials,
    /// Object storage for backups.
    BackupRepository,
    /// Object storage the cluster was restored from.
    RestoreRepository,
    /// Referenced by a custom environment variable.
    CustomEnv,
    /// Holds initialisation scripts.
    InitScripts,
    /// Server certificate for TLS (kubernetes.io/tls Secret).
    Tls,
    /// Password for a declared role.
    RolePassword,
}

#[cfg_attr(feature = "crd", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RequiredResource {
    pub name: String,
    pub kind: ResourceKind,
    pub purpose: ResourcePurpose,
    /// Keys that must be present. Empty when any key will do (init scripts).
    pub keys: Vec<String>,
    /// pglord creates and owns this one; the user creates the rest.
    pub managed: bool,
}

/// Everything the spec says this cluster depends on, deduplicated by name.
///
/// One object can be needed twice — the same Secret backing two environment
/// variables, say — and the required keys are merged rather than listed twice.
pub fn required_resources(spec: &ClusterSpec) -> Vec<RequiredResource> {
    let mut out: Vec<RequiredResource> = Vec::new();

    let mut push = |resource: RequiredResource| match out
        .iter_mut()
        .find(|r| r.name == resource.name && r.kind == resource.kind)
    {
        Some(existing) => {
            for key in resource.keys {
                if !existing.keys.contains(&key) {
                    existing.keys.push(key);
                }
            }
        }
        None => out.push(resource),
    };

    // Credentials: generated unless the user pointed the cluster at their own,
    // in which case they own it and must supply every key. The pooler key is
    // only demanded when the pooler actually reads it.
    let custom = spec.credentials_secret();
    let mut credential_keys = vec![
        SUPERUSER_PASSWORD_KEY.to_string(),
        REPLICATION_PASSWORD_KEY.to_string(),
    ];
    if spec.pooler.enabled {
        credential_keys.push(POOLER_PASSWORD_KEY.to_string());
    }
    push(RequiredResource {
        name: custom
            .map(str::to_string)
            .unwrap_or_else(|| credentials_secret_name(&spec.name)),
        kind: ResourceKind::Secret,
        purpose: ResourcePurpose::Credentials,
        keys: credential_keys,
        managed: custom.is_none(),
    });

    if spec.backup.enabled
        && let Some(name) = spec.backup.s3_credentials_secret.as_deref()
    {
        push(RequiredResource {
            name: name.to_string(),
            kind: ResourceKind::Secret,
            purpose: ResourcePurpose::BackupRepository,
            keys: vec![ACCESS_KEY_KEY.to_string(), SECRET_KEY_KEY.to_string()],
            managed: false,
        });
    }

    if let Some(restore) = &spec.restore
        && let Some(name) = restore.repository.s3_credentials_secret.as_deref()
    {
        push(RequiredResource {
            name: name.to_string(),
            kind: ResourceKind::Secret,
            purpose: ResourcePurpose::RestoreRepository,
            keys: vec![ACCESS_KEY_KEY.to_string(), SECRET_KEY_KEY.to_string()],
            managed: false,
        });
    }

    for env in &spec.env {
        if let Some(secret) = &env.secret {
            push(RequiredResource {
                name: secret.name.clone(),
                kind: ResourceKind::Secret,
                purpose: ResourcePurpose::CustomEnv,
                keys: vec![secret.key.clone()],
                managed: false,
            });
        }
    }

    for role in &spec.roles {
        if let Some(secret) = &role.password_secret {
            push(RequiredResource {
                name: secret.name.clone(),
                kind: ResourceKind::Secret,
                purpose: ResourcePurpose::RolePassword,
                keys: vec![secret.key.clone()],
                managed: false,
            });
        }
    }

    if let Some(name) = spec.init.config_map() {
        push(RequiredResource {
            name: name.to_string(),
            kind: ResourceKind::ConfigMap,
            purpose: ResourcePurpose::InitScripts,
            keys: Vec::new(),
            managed: false,
        });
    }
    if let Some(name) = spec.init.secret() {
        push(RequiredResource {
            name: name.to_string(),
            kind: ResourceKind::Secret,
            purpose: ResourcePurpose::InitScripts,
            keys: Vec::new(),
            managed: false,
        });
    }

    if let Some(name) = spec.tls.server_secret() {
        let mut keys = vec![
            crate::tls::TLS_CERT_KEY.to_string(),
            crate::tls::TLS_KEY_KEY.to_string(),
        ];
        // The CA bundle is optional for plain TLS but load-bearing for cert
        // auth: postgres cannot verify a client certificate without it.
        if spec
            .pg_hba
            .iter()
            .any(|r| r.method == crate::hba::HbaMethod::Cert)
        {
            keys.push(crate::tls::TLS_CA_KEY.to_string());
        }
        push(RequiredResource {
            name: name.to_string(),
            kind: ResourceKind::Secret,
            purpose: ResourcePurpose::Tls,
            keys,
            managed: false,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: serde_json::Value) -> ClusterSpec {
        serde_json::from_value(json).expect("spec")
    }

    #[test]
    fn a_plain_cluster_needs_only_its_generated_credentials() {
        let r = required_resources(&spec(serde_json::json!({ "name": "demo" })));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "demo-credentials");
        assert_eq!(r[0].purpose, ResourcePurpose::Credentials);
        assert!(r[0].managed, "pglord creates this one");
        assert_eq!(
            r[0].keys,
            vec!["superuser-password", "replication-password"]
        );
    }

    #[test]
    fn pointing_at_your_own_credentials_makes_them_yours_to_create() {
        let r = required_resources(&spec(serde_json::json!({
            "name": "demo",
            "credentials_secret": "my-creds"
        })));
        assert_eq!(r[0].name, "my-creds");
        assert!(
            !r[0].managed,
            "pglord must not claim to create a secret the user supplied"
        );
        // Same keys either way: the agent reads them by name.
        assert!(r[0].keys.contains(&"superuser-password".to_string()));
    }

    #[test]
    fn backup_and_init_sources_are_listed_with_their_kinds() {
        let r = required_resources(&spec(serde_json::json!({
            "name": "demo",
            "backup": {
                "enabled": true,
                "s3_endpoint": "https://s3.example.com",
                "s3_bucket": "b",
                "s3_credentials_secret": "backup-creds"
            },
            "init": { "scripts_config_map": "init-sql", "scripts_secret": "init-secret" }
        })));
        let find = |name: &str| r.iter().find(|x| x.name == name).expect(name).clone();

        let backup = find("backup-creds");
        assert_eq!(backup.purpose, ResourcePurpose::BackupRepository);
        assert_eq!(backup.keys, vec!["access-key", "secret-key"]);
        assert!(!backup.managed);

        assert_eq!(find("init-sql").kind, ResourceKind::ConfigMap);
        assert_eq!(find("init-secret").kind, ResourceKind::Secret);
        // Any .sql key will do, so no key list to check against.
        assert!(find("init-sql").keys.is_empty());
    }

    #[test]
    fn a_disabled_backup_repository_is_not_required() {
        // Listing a secret the cluster does not actually read would send people
        // off to create something that is never used.
        let r = required_resources(&spec(serde_json::json!({
            "name": "demo",
            "backup": { "enabled": false, "s3_credentials_secret": "backup-creds" }
        })));
        assert!(!r.iter().any(|x| x.name == "backup-creds"));
    }

    #[test]
    fn one_secret_backing_two_variables_is_listed_once_with_both_keys() {
        let r = required_resources(&spec(serde_json::json!({
            "name": "demo",
            "env": [
                { "name": "APP_USER", "secret": { "name": "app", "key": "user" } },
                { "name": "APP_PASSWORD", "secret": { "name": "app", "key": "password" } },
                { "name": "LITERAL", "value": "x" }
            ]
        })));
        let app: Vec<_> = r.iter().filter(|x| x.name == "app").collect();
        assert_eq!(app.len(), 1, "one entry per object, not per reference");
        assert_eq!(app[0].keys, vec!["user", "password"]);
        // A literal value needs nothing from the namespace.
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn a_tls_cluster_needs_its_cert_secret_and_cert_rules_add_the_ca() {
        let r = required_resources(&spec(serde_json::json!({
            "name": "demo",
            "tls": { "server_secret": "demo-tls" }
        })));
        let tls = r.iter().find(|x| x.name == "demo-tls").expect("demo-tls");
        assert_eq!(tls.kind, ResourceKind::Secret);
        assert_eq!(tls.purpose, ResourcePurpose::Tls);
        assert_eq!(tls.keys, vec!["tls.crt", "tls.key"]);
        assert!(!tls.managed);

        let r = required_resources(&spec(serde_json::json!({
            "name": "demo",
            "tls": { "server_secret": "demo-tls" },
            "pg_hba": [{ "conn": "hostssl", "database": "app", "user": "app_rw",
                         "address": "10.0.0.0/8", "method": "cert" }]
        })));
        let tls = r.iter().find(|x| x.name == "demo-tls").expect("demo-tls");
        assert_eq!(tls.keys, vec!["tls.crt", "tls.key", "ca.crt"]);

        // No TLS, no requirement.
        let r = required_resources(&spec(serde_json::json!({ "name": "demo" })));
        assert!(!r.iter().any(|x| x.purpose == ResourcePurpose::Tls));
    }

    #[test]
    fn declared_role_password_secrets_are_required() {
        let r = required_resources(&spec(serde_json::json!({
            "name": "demo",
            "roles": [
                { "name": "app_rw", "password_secret": { "name": "app-creds", "key": "password" } },
                { "name": "app_ro" }
            ]
        })));
        let creds = r.iter().find(|x| x.name == "app-creds").expect("app-creds");
        assert_eq!(creds.purpose, ResourcePurpose::RolePassword);
        assert_eq!(creds.keys, vec!["password"]);
        assert!(!creds.managed);
        // A role without a reference demands nothing from the namespace.
        assert_eq!(
            r.iter()
                .filter(|x| x.purpose == ResourcePurpose::RolePassword)
                .count(),
            1
        );
    }

    #[test]
    fn an_enabled_pooler_requires_the_pooler_password_key() {
        let r = required_resources(&spec(serde_json::json!({
            "name": "demo",
            "credentials_secret": "my-creds",
            "pooler": { "enabled": true }
        })));
        assert!(r[0].keys.contains(&"pooler-password".to_string()));

        // No pooler, no extra key to send someone off to create.
        let r = required_resources(&spec(serde_json::json!({ "name": "demo" })));
        assert!(!r[0].keys.contains(&"pooler-password".to_string()));
    }

    #[test]
    fn a_restored_cluster_still_needs_the_repository_it_reads() {
        // Its own backups are off, but bootstrap fails without these.
        let r = required_resources(&spec(serde_json::json!({
            "name": "copy",
            "restore": {
                "source_cluster": "src",
                "repository": {
                    "enabled": true,
                    "s3_endpoint": "https://s3.example.com",
                    "s3_bucket": "b",
                    "s3_credentials_secret": "src-creds"
                }
            }
        })));
        let src = r.iter().find(|x| x.name == "src-creds").expect("src-creds");
        assert_eq!(src.purpose, ResourcePurpose::RestoreRepository);
        assert!(!src.managed);
    }
}
