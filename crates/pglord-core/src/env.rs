//! Custom environment variables on the instance containers.
//!
//! These exist so init scripts (and anything else in the container) can reach
//! values the user controls, including ones that must come from a Secret.
//!
//! The reserved list is the important part. The agent takes its entire
//! configuration from `PGLORD_AGENT_*`, so an unguarded env var here would let
//! anyone who can edit a cluster spec set `PGLORD_AGENT_ROLE=primary` on every
//! instance and split-brain the cluster. libpq's variables are equally
//! dangerous in a quieter way: `PGDATA` would point postgres at another
//! directory, and the agent's own psql calls read `PGHOST`/`PGUSER`.

use serde::{Deserialize, Serialize};

/// Prefixes owned by pglord. Nothing under these may be set by a user.
pub const RESERVED_ENV_PREFIXES: &[&str] = &["PGLORD_", "PGBACKREST_"];

/// Individually reserved names: libpq and postgres read these, and the agent
/// shells out to psql/pg_ctl which would inherit them.
pub const RESERVED_ENV_NAMES: &[&str] = &[
    "PGDATA",
    "PGHOST",
    "PGHOSTADDR",
    "PGPORT",
    "PGUSER",
    "PGPASSWORD",
    "PGPASSFILE",
    "PGDATABASE",
    "PGOPTIONS",
    "PGSERVICE",
    "PGSERVICEFILE",
    "PGSSLMODE",
    "PGSSLROOTCERT",
    "PGREQUIRESSL",
    "PGCONNECT_TIMEOUT",
    "POSTGRES_USER",
    "POSTGRES_PASSWORD",
    "POSTGRES_DB",
    "POSTGRES_INITDB_ARGS",
    "PATH",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
];

#[cfg_attr(feature = "crd", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SecretKeyRefSpec {
    /// Secret in the same namespace. pglord never reads its contents.
    pub name: String,
    pub key: String,
}

#[cfg_attr(feature = "crd", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct EnvVarSpec {
    pub name: String,
    /// Literal value. Mutually exclusive with `secret`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Value taken from a Secret at pod start. Mutually exclusive with `value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<SecretKeyRefSpec>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EnvError {
    #[error(
        "invalid environment variable name '{0}': use letters, digits and underscores, \
         not starting with a digit"
    )]
    InvalidName(String),
    #[error("'{0}' is reserved by pglord and cannot be set")]
    Reserved(String),
    #[error("environment variable '{0}' needs either a value or a secret reference, not both")]
    AmbiguousSource(String),
    #[error("environment variable '{0}' has no value and no secret reference")]
    NoSource(String),
    #[error("environment variable '{name}' references a secret without a {missing}")]
    IncompleteSecretRef { name: String, missing: &'static str },
    #[error("environment variable '{0}' is defined more than once")]
    Duplicate(String),
}

pub fn is_reserved(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    RESERVED_ENV_PREFIXES.iter().any(|p| upper.starts_with(p))
        || RESERVED_ENV_NAMES.contains(&upper.as_str())
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn validate(env: &[EnvVarSpec]) -> Result<(), EnvError> {
    let mut seen: Vec<&str> = Vec::with_capacity(env.len());
    for e in env {
        if !is_valid_name(&e.name) {
            return Err(EnvError::InvalidName(e.name.clone()));
        }
        if is_reserved(&e.name) {
            return Err(EnvError::Reserved(e.name.clone()));
        }
        if seen.contains(&e.name.as_str()) {
            return Err(EnvError::Duplicate(e.name.clone()));
        }
        seen.push(&e.name);
        match (&e.value, &e.secret) {
            (Some(_), Some(_)) => return Err(EnvError::AmbiguousSource(e.name.clone())),
            (None, None) => return Err(EnvError::NoSource(e.name.clone())),
            (None, Some(s)) => {
                if s.name.trim().is_empty() {
                    return Err(EnvError::IncompleteSecretRef {
                        name: e.name.clone(),
                        missing: "secret name",
                    });
                }
                if s.key.trim().is_empty() {
                    return Err(EnvError::IncompleteSecretRef {
                        name: e.name.clone(),
                        missing: "key",
                    });
                }
            }
            (Some(_), None) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(name: &str) -> EnvVarSpec {
        EnvVarSpec {
            name: name.to_string(),
            value: Some("v".into()),
            secret: None,
        }
    }

    fn from_secret(name: &str, secret: &str, key: &str) -> EnvVarSpec {
        EnvVarSpec {
            name: name.to_string(),
            value: None,
            secret: Some(SecretKeyRefSpec {
                name: secret.to_string(),
                key: key.to_string(),
            }),
        }
    }

    #[test]
    fn ordinary_variables_are_accepted() {
        assert_eq!(
            validate(&[literal("APP_USER"), from_secret("APP_PASSWORD", "s", "k")]),
            Ok(())
        );
        assert_eq!(validate(&[]), Ok(()));
    }

    #[test]
    fn agent_configuration_cannot_be_hijacked() {
        // The agent reads its whole configuration from PGLORD_AGENT_*. Setting
        // the role here would let one spec edit declare every instance a
        // primary, which is a split brain with no recovery path.
        assert_eq!(
            validate(&[literal("PGLORD_AGENT_ROLE")]),
            Err(EnvError::Reserved("PGLORD_AGENT_ROLE".into()))
        );
        assert_eq!(
            validate(&[literal("PGLORD_AGENT_PRIMARY_HOST")]),
            Err(EnvError::Reserved("PGLORD_AGENT_PRIMARY_HOST".into()))
        );
        // Object-storage credentials are injected by the reconciler; letting a
        // spec override them would redirect backups to someone else's bucket.
        assert_eq!(
            validate(&[literal("PGBACKREST_REPO1_S3_KEY")]),
            Err(EnvError::Reserved("PGBACKREST_REPO1_S3_KEY".into()))
        );
    }

    #[test]
    fn postgres_and_libpq_variables_are_reserved() {
        // PGDATA would point postgres somewhere else entirely; the rest are
        // inherited by the agent's own psql and pg_ctl calls.
        for name in ["PGDATA", "PGHOST", "PGUSER", "PGPASSWORD", "PGPORT", "PATH"] {
            assert_eq!(
                validate(&[literal(name)]),
                Err(EnvError::Reserved(name.to_string())),
                "expected {name} to be reserved"
            );
        }
    }

    #[test]
    fn reserved_check_is_case_insensitive() {
        // Environment lookups are case-sensitive on Linux, but accepting
        // `pglord_agent_role` would still be a trap for whoever reads the spec
        // later, and some tooling upper-cases names.
        assert!(is_reserved("pgdata"));
        assert!(is_reserved("pglord_agent_role"));
        assert!(!is_reserved("APP_PGDATA"));
    }

    #[test]
    fn a_variable_needs_exactly_one_source() {
        let both = EnvVarSpec {
            name: "APP".into(),
            value: Some("v".into()),
            secret: Some(SecretKeyRefSpec {
                name: "s".into(),
                key: "k".into(),
            }),
        };
        assert_eq!(
            validate(&[both]),
            Err(EnvError::AmbiguousSource("APP".into()))
        );

        let neither = EnvVarSpec {
            name: "APP".into(),
            value: None,
            secret: None,
        };
        assert_eq!(validate(&[neither]), Err(EnvError::NoSource("APP".into())));
    }

    #[test]
    fn secret_references_must_name_a_secret_and_a_key() {
        // A blank key produces a pod that never starts, with the reason buried
        // in pod events rather than reported at the API.
        assert!(matches!(
            validate(&[from_secret("APP", "", "k")]),
            Err(EnvError::IncompleteSecretRef { .. })
        ));
        assert!(matches!(
            validate(&[from_secret("APP", "s", "  ")]),
            Err(EnvError::IncompleteSecretRef { .. })
        ));
    }

    #[test]
    fn names_must_be_usable_as_environment_variables() {
        for bad in ["", "1APP", "APP-NAME", "APP NAME", "APP.NAME"] {
            assert!(
                matches!(validate(&[literal(bad)]), Err(EnvError::InvalidName(_))),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn duplicates_are_rejected_rather_than_silently_shadowed() {
        // Kubernetes keeps the last of a duplicated env entry; a spec that
        // quietly does something other than what it reads is worse than an
        // error at the API.
        assert_eq!(
            validate(&[literal("APP"), literal("APP")]),
            Err(EnvError::Duplicate("APP".into()))
        );
    }
}
