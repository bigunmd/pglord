//! TLS for client and replication connections.
//!
//! pglord never generates key material: the spec references a standard
//! `kubernetes.io/tls` Secret (tls.crt/tls.key, optional ca.crt) that
//! something else — typically cert-manager — creates and renews. The
//! reconciler mounts it read-only; the agent turns it into a managed conf
//! include and reloads postgres when the mounted files change.

use serde::{Deserialize, Serialize};

/// Where the reconciler mounts the server certificate Secret.
pub const TLS_MOUNT: &str = "/etc/pglord/tls";
pub const TLS_VOLUME: &str = "tls-certs";
/// Keys of a standard kubernetes.io/tls Secret.
pub const TLS_CERT_KEY: &str = "tls.crt";
pub const TLS_KEY_KEY: &str = "tls.key";
/// Optional CA bundle key (cert-manager writes it); required for `cert` auth.
pub const TLS_CA_KEY: &str = "ca.crt";

#[cfg_attr(feature = "crd", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TlsSpec {
    /// kubernetes.io/tls Secret holding the server certificate. TLS is
    /// enabled iff this is set; clearing it disables TLS. Both directions
    /// change the pod template and roll the instances.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_secret: Option<String>,
}

impl TlsSpec {
    /// The Secret name, ignoring blanks a form might send.
    pub fn server_secret(&self) -> Option<&str> {
        self.server_secret
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn enabled(&self) -> bool {
        self.server_secret().is_some()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TlsError {
    #[error("invalid TLS secret name '{0}': must be a lowercase RFC1123 name")]
    InvalidSecretName(String),
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

pub fn validate(t: &TlsSpec) -> Result<(), TlsError> {
    if let Some(name) = t.server_secret()
        && !is_rfc1123_subdomain(name)
    {
        return Err(TlsError::InvalidSecretName(name.to_string()));
    }
    Ok(())
}

/// The managed conf include. `enabled` must already account for the cert
/// files being present on disk (the agent guards this): postgres refuses to
/// *start* with `ssl = on` and no certificate, so a pod whose spec says TLS
/// but whose template has not rolled to carry the mount yet must keep ssl off.
pub fn conf(enabled: bool, ca_present: bool) -> String {
    let mut out =
        String::from("# Managed by pglord — do not edit; changes come from the cluster spec.\n");
    if enabled {
        out.push_str("ssl = on\n");
        out.push_str(&format!("ssl_cert_file = '{TLS_MOUNT}/{TLS_CERT_KEY}'\n"));
        out.push_str(&format!("ssl_key_file = '{TLS_MOUNT}/{TLS_KEY_KEY}'\n"));
        if ca_present {
            out.push_str(&format!("ssl_ca_file = '{TLS_MOUNT}/{TLS_CA_KEY}'\n"));
        }
    } else {
        // Explicit off (not an absent file) so disabling TLS clears the
        // setting on reload rather than leaving the old value in effect.
        out.push_str("ssl = off\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: Option<&str>) -> TlsSpec {
        TlsSpec {
            server_secret: name.map(str::to_string),
        }
    }

    #[test]
    fn absent_and_blank_names_count_as_disabled() {
        assert!(!TlsSpec::default().enabled());
        // A form that sends "" for an untouched field must not enable TLS
        // against a Secret named "".
        assert!(!spec(Some("  ")).enabled());
        assert_eq!(validate(&spec(Some(""))), Ok(()));
        assert!(spec(Some("mydb-tls")).enabled());
    }

    #[test]
    fn secret_name_must_be_a_valid_kubernetes_object_name() {
        assert_eq!(validate(&spec(Some("mydb-tls"))), Ok(()));
        assert_eq!(validate(&spec(Some("a.b-c"))), Ok(()));
        for bad in ["My-TLS", "tls_secret", "-tls", "tls-"] {
            assert!(
                matches!(
                    validate(&spec(Some(bad))),
                    Err(TlsError::InvalidSecretName(_))
                ),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn enabled_conf_points_at_the_mounted_secret_keys() {
        assert_eq!(
            conf(true, false),
            "# Managed by pglord — do not edit; changes come from the cluster spec.\n\
             ssl = on\n\
             ssl_cert_file = '/etc/pglord/tls/tls.crt'\n\
             ssl_key_file = '/etc/pglord/tls/tls.key'\n"
        );
    }

    #[test]
    fn ca_line_appears_only_when_the_ca_file_exists() {
        let with_ca = conf(true, true);
        assert!(with_ca.contains("ssl_ca_file = '/etc/pglord/tls/ca.crt'\n"));
        assert!(!conf(true, false).contains("ssl_ca_file"));
    }

    #[test]
    fn disabled_renders_an_explicit_off() {
        let off = conf(false, false);
        assert!(off.contains("ssl = off\n"));
        assert!(!off.contains("ssl_cert_file"));
        // ca_present without enabled must not sneak a CA line in.
        assert_eq!(off, conf(false, true));
    }
}
