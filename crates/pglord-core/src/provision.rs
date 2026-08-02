//! Declarative roles and databases, converged by the primary's agent.
//!
//! The spec is additive: pglord creates and alters what is declared and never
//! drops what isn't — removing an entry stops managing it, nothing else. No
//! DROP exists anywhere in this path. Passwords are referenced Secrets, never
//! spec values: the spec is a plain ConfigMap readable by anything with
//! namespace read access (same reasoning as init scripts).

use serde::{Deserialize, Serialize};

use crate::env::SecretKeyRefSpec;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RoleSpec {
    /// Strict lowercase identifier — interpolated into DDL as a quoted
    /// identifier, so the charset is the injection contract.
    pub name: String,
    /// Where the role's password comes from. Absent = the role is created
    /// without a password and an existing password is left untouched.
    /// Rotation is: edit the Secret; the next ensure re-applies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_secret: Option<SecretKeyRefSpec>,
    #[serde(default = "default_true")]
    pub login: bool,
    #[serde(default)]
    pub createdb: bool,
    #[serde(default)]
    pub createrole: bool,
    /// Absent = unlimited (postgres -1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_limit: Option<i32>,
    /// Memberships. Only declared roles can be named, which keeps ensure
    /// ordering trivial and revocation scoped to what pglord manages.
    #[serde(default)]
    pub in_roles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DatabaseSpec {
    pub name: String,
    /// Owning role; must be declared above.
    pub owner: String,
}

fn default_true() -> bool {
    true
}

/// Role names pglord refuses to manage: its own machinery and everything
/// postgres reserves.
const RESERVED_ROLES: [&str; 3] = ["postgres", "replicator", "pglord_pooler"];
const RESERVED_DATABASES: [&str; 3] = ["postgres", "template0", "template1"];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProvisionError {
    #[error("invalid name '{0}': must match [a-z_][a-z0-9_]* and be at most 63 bytes")]
    InvalidName(String),
    #[error("'{0}' is reserved and cannot be managed through the spec")]
    ReservedName(String),
    #[error("'{0}' is declared more than once")]
    DuplicateName(String),
    #[error("'{referrer}' references role '{role}', which is not declared")]
    UnknownRole { referrer: String, role: String },
    #[error("role '{role}' has a password secret reference without a name or key")]
    InvalidSecretRef { role: String },
    #[error("role '{role}' has connection limit {limit}: must be -1 (unlimited) or >= 0")]
    InvalidConnectionLimit { role: String, limit: i32 },
}

/// A name safe to interpolate into DDL as a quoted identifier: nothing in
/// this charset can close a quote or smuggle syntax.
pub fn is_valid_pg_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub fn validate(roles: &[RoleSpec], databases: &[DatabaseSpec]) -> Result<(), ProvisionError> {
    let mut role_names: Vec<&str> = Vec::with_capacity(roles.len());
    for role in roles {
        if !is_valid_pg_name(&role.name) {
            return Err(ProvisionError::InvalidName(role.name.clone()));
        }
        if RESERVED_ROLES.contains(&role.name.as_str()) || role.name.starts_with("pg_") {
            return Err(ProvisionError::ReservedName(role.name.clone()));
        }
        if role_names.contains(&role.name.as_str()) {
            return Err(ProvisionError::DuplicateName(role.name.clone()));
        }
        if let Some(sref) = &role.password_secret
            && (sref.name.trim().is_empty() || sref.key.trim().is_empty())
        {
            return Err(ProvisionError::InvalidSecretRef {
                role: role.name.clone(),
            });
        }
        if let Some(limit) = role.connection_limit
            && limit < -1
        {
            return Err(ProvisionError::InvalidConnectionLimit {
                role: role.name.clone(),
                limit,
            });
        }
        role_names.push(&role.name);
    }
    for role in roles {
        for member_of in &role.in_roles {
            if !role_names.contains(&member_of.as_str()) {
                return Err(ProvisionError::UnknownRole {
                    referrer: role.name.clone(),
                    role: member_of.clone(),
                });
            }
        }
    }
    let mut db_names: Vec<&str> = Vec::with_capacity(databases.len());
    for db in databases {
        if !is_valid_pg_name(&db.name) {
            return Err(ProvisionError::InvalidName(db.name.clone()));
        }
        if RESERVED_DATABASES.contains(&db.name.as_str()) {
            return Err(ProvisionError::ReservedName(db.name.clone()));
        }
        if db_names.contains(&db.name.as_str()) {
            return Err(ProvisionError::DuplicateName(db.name.clone()));
        }
        if !role_names.contains(&db.owner.as_str()) {
            return Err(ProvisionError::UnknownRole {
                referrer: db.name.clone(),
                role: db.owner.clone(),
            });
        }
        db_names.push(&db.name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(name: &str) -> RoleSpec {
        serde_json::from_value(serde_json::json!({ "name": name })).expect("role")
    }

    #[test]
    fn defaults_are_login_only() {
        let r = role("app_rw");
        assert!(r.login && !r.createdb && !r.createrole);
        assert!(r.password_secret.is_none() && r.in_roles.is_empty());
        assert!(r.connection_limit.is_none());
    }

    #[test]
    fn names_are_strict_identifiers() {
        let long = "x".repeat(64);
        for bad in ["", "App", "1st", "has-dash", "has space", long.as_str()] {
            assert!(
                matches!(
                    validate(&[role(bad)], &[]),
                    Err(ProvisionError::InvalidName(_))
                ),
                "{bad:?} accepted"
            );
        }
        assert!(validate(&[role("app_rw2")], &[]).is_ok());
        assert!(validate(&[role("_private")], &[]).is_ok());
    }

    #[test]
    fn reserved_names_are_rejected() {
        for bad in ["postgres", "replicator", "pglord_pooler", "pg_monitor"] {
            assert!(
                matches!(
                    validate(&[role(bad)], &[]),
                    Err(ProvisionError::ReservedName(_))
                ),
                "{bad} accepted"
            );
        }
        for bad in ["postgres", "template0", "template1"] {
            let db = DatabaseSpec {
                name: bad.into(),
                owner: "app_rw".into(),
            };
            assert!(
                matches!(
                    validate(&[role("app_rw")], &[db]),
                    Err(ProvisionError::ReservedName(_))
                ),
                "db {bad} accepted"
            );
        }
    }

    #[test]
    fn references_must_name_declared_roles() {
        let mut r = role("app_rw");
        r.in_roles = vec!["reporting".into()];
        assert!(matches!(
            validate(&[r], &[]),
            Err(ProvisionError::UnknownRole { .. })
        ));
        let db = DatabaseSpec {
            name: "appdb".into(),
            owner: "ghost".into(),
        };
        assert!(matches!(
            validate(&[role("app_rw")], &[db]),
            Err(ProvisionError::UnknownRole { .. })
        ));
    }

    #[test]
    fn duplicates_and_bad_limits_are_rejected() {
        assert!(matches!(
            validate(&[role("a"), role("a")], &[]),
            Err(ProvisionError::DuplicateName(_))
        ));
        let mut r = role("a");
        r.connection_limit = Some(-2);
        assert!(matches!(
            validate(&[r], &[]),
            Err(ProvisionError::InvalidConnectionLimit { .. })
        ));
        let mut r = role("a");
        r.connection_limit = Some(-1);
        assert!(
            validate(&[r], &[]).is_ok(),
            "-1 is postgres's own unlimited"
        );
    }

    #[test]
    fn empty_secret_refs_are_rejected() {
        let mut r = role("a");
        r.password_secret = Some(crate::env::SecretKeyRefSpec {
            name: " ".into(),
            key: "password".into(),
        });
        assert!(matches!(
            validate(&[r], &[]),
            Err(ProvisionError::InvalidSecretRef { .. })
        ));
    }
}
