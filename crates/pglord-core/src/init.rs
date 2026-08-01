//! Initialisation scripts run once, when a cluster's data directory is first
//! created.
//!
//! Scripts come from a ConfigMap (and optionally a Secret, for init SQL that
//! contains credentials) rather than living in the spec: the spec is a plain
//! ConfigMap readable by anything with namespace read access, so SQL that
//! creates a role with a password does not belong in it.
//!
//! SQL only. Shell scripts would hand anyone who can write that ConfigMap
//! arbitrary code execution as the postgres user in every instance.

use serde::{Deserialize, Serialize};

/// Where the reconciler mounts the script sources inside the container.
pub const SCRIPTS_CONFIG_MAP_MOUNT: &str = "/etc/pglord/initdb.d/configmap";
pub const SCRIPTS_SECRET_MOUNT: &str = "/etc/pglord/initdb.d/secret";
pub const SCRIPTS_CONFIG_MAP_VOLUME: &str = "init-scripts-configmap";
pub const SCRIPTS_SECRET_VOLUME: &str = "init-scripts-secret";

/// Marker written into PGDATA once the initialisation phase is over. Present
/// on every instance that has been through bootstrap, whether or not any
/// scripts were configured — adding scripts to a running cluster must not
/// re-run initialisation against data that already exists.
pub const INIT_MARKER: &str = "pglord.init-complete";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct InitSpec {
    /// ConfigMap whose `.sql` keys are run in filename order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scripts_config_map: Option<String>,
    /// Secret whose `.sql` keys are run after the ConfigMap's, in filename
    /// order. For init SQL that contains credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scripts_secret: Option<String>,
}

impl InitSpec {
    pub fn is_empty(&self) -> bool {
        self.config_map().is_none() && self.secret().is_none()
    }

    /// The ConfigMap name, ignoring blanks a form might send.
    pub fn config_map(&self) -> Option<&str> {
        self.scripts_config_map
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn secret(&self) -> Option<&str> {
        self.scripts_secret
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InitError {
    #[error("invalid {what} name '{name}': must be a lowercase RFC1123 name")]
    InvalidName { what: &'static str, name: String },
}

fn is_rfc1123_subdomain(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|label| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

pub fn validate(i: &InitSpec) -> Result<(), InitError> {
    for (what, name) in [("ConfigMap", i.config_map()), ("Secret", i.secret())] {
        if let Some(name) = name
            && !is_rfc1123_subdomain(name)
        {
            return Err(InitError::InvalidName {
                what,
                name: name.to_string(),
            });
        }
    }
    Ok(())
}

/// Directories to read scripts from, in the order they run.
pub fn script_dirs(i: &InitSpec) -> Vec<&'static str> {
    let mut dirs = Vec::new();
    if i.config_map().is_some() {
        dirs.push(SCRIPTS_CONFIG_MAP_MOUNT);
    }
    if i.secret().is_some() {
        dirs.push(SCRIPTS_SECRET_MOUNT);
    }
    dirs
}

/// Whether a file mounted from a ConfigMap/Secret is a script to run.
///
/// Kubernetes projects keys into `..data`-linked files; the dotfiles it creates
/// alongside them must never be executed.
pub fn is_script(file_name: &str) -> bool {
    !file_name.starts_with('.') && file_name.ends_with(".sql")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(cm: Option<&str>, secret: Option<&str>) -> InitSpec {
        InitSpec {
            scripts_config_map: cm.map(str::to_string),
            scripts_secret: secret.map(str::to_string),
        }
    }

    #[test]
    fn nothing_configured_is_valid_and_empty() {
        assert_eq!(validate(&InitSpec::default()), Ok(()));
        assert!(InitSpec::default().is_empty());
        assert!(script_dirs(&InitSpec::default()).is_empty());
    }

    #[test]
    fn blank_names_count_as_absent() {
        // A form that sends "" for an untouched field must not produce a
        // volume mount for a ConfigMap named "".
        let s = spec(Some("  "), Some(""));
        assert!(s.is_empty());
        assert_eq!(validate(&s), Ok(()));
        assert!(script_dirs(&s).is_empty());
    }

    #[test]
    fn configmap_scripts_run_before_secret_scripts() {
        let s = spec(Some("init-sql"), Some("init-secret"));
        assert_eq!(
            script_dirs(&s),
            vec![SCRIPTS_CONFIG_MAP_MOUNT, SCRIPTS_SECRET_MOUNT]
        );
        // Either source alone works.
        assert_eq!(
            script_dirs(&spec(None, Some("s"))),
            vec![SCRIPTS_SECRET_MOUNT]
        );
    }

    #[test]
    fn names_must_be_valid_kubernetes_object_names() {
        assert_eq!(validate(&spec(Some("init-sql"), None)), Ok(()));
        for bad in ["Init-SQL", "init_sql", "-init", "init-"] {
            assert!(
                matches!(
                    validate(&spec(Some(bad), None)),
                    Err(InitError::InvalidName { .. })
                ),
                "expected rejection for {bad:?}"
            );
        }
        assert!(validate(&spec(None, Some("Bad_Name"))).is_err());
    }

    #[test]
    fn only_sql_files_run_and_never_kubernetes_dotfiles() {
        assert!(is_script("01-schema.sql"));
        // Kubernetes projects keys through ..data and ..2026_08_01 symlinks;
        // running those would execute the same script twice at best.
        assert!(!is_script("..data"));
        assert!(!is_script("..2026_08_01_12_00_00.actual"));
        assert!(!is_script(".hidden.sql"));
        // Shell is deliberately not supported.
        assert!(!is_script("seed.sh"));
        assert!(!is_script("notes.txt"));
    }
}
