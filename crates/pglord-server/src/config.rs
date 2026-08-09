//! Startup configuration. Layered: serde defaults <- config.toml <- PGLORD_* env.

use std::net::SocketAddr;
use std::time::Duration;

use axum::http::HeaderValue;
use figment::Figment;
use figment::providers::{Env, Format, Toml};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Listen address for the HTTP server.
    #[serde(default = "default_http_addr")]
    pub http_addr: SocketAddr,
    /// tracing-subscriber EnvFilter directive, e.g. "info" or "pglord_server=debug,info".
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Graceful drain budget; below Kubernetes' default 30s termination grace period.
    #[serde(default = "default_shutdown_timeout_secs")]
    pub shutdown_timeout_secs: u64,
    /// Comma-separated exact origins allowed by CORS. Empty = no CORS layer.
    #[serde(default = "default_empty")]
    pub cors_allowed_origins: String,
    /// Image used for Postgres instances when a cluster does not name one.
    /// The built-in default only exists on a development machine, so a real
    /// deployment has to set this or every cluster lands in ImagePullBackOff.
    #[serde(default = "default_agent_image")]
    pub default_agent_image: String,
    /// Comma-separated static API bearer tokens. Empty = static mode off.
    /// Two entries make rotation zero-downtime: add new, roll clients, drop old.
    #[serde(default = "default_empty")]
    pub auth_tokens: String,
    /// OIDC issuer URL; discovery is fetched from
    /// `<issuer>/.well-known/openid-configuration`. Empty = OIDC off.
    #[serde(default = "default_empty")]
    pub auth_oidc_issuer: String,
    /// Client ID the SPA uses; served to the UI via /api/v1/auth/config.
    #[serde(default = "default_empty")]
    pub auth_oidc_client_id: String,
    /// Expected `aud` claim. Empty = defaults to the client ID.
    #[serde(default = "default_empty")]
    pub auth_oidc_audience: String,
    /// Accept Kubernetes bearer tokens: TokenReview authenticates them and a
    /// SubjectAccessReview on pglord.dev/postgresclusters authorizes each
    /// request. Needs the system:auth-delegator binding on the server's
    /// ServiceAccount.
    #[serde(default)]
    pub auth_kubernetes: bool,
    /// Authorize OIDC credentials through a SubjectAccessReview on
    /// pglord.dev/postgresclusters instead of granting them full admin. A
    /// SAR evaluates RBAC against whatever subject strings it is handed, so
    /// the apiserver does not have to know the IdP — operators just bind
    /// Roles to the mapped identities. Needs the auth-delegator binding.
    #[serde(default)]
    pub auth_oidc_rbac: bool,
    /// Claim carrying the username; commonly `email` or `preferred_username`.
    #[serde(default = "default_username_claim")]
    pub auth_oidc_username_claim: String,
    /// Prepended to the mapped username. Mirror the apiserver's
    /// --oidc-username-prefix to match kubectl exactly. Empty = no prefix.
    #[serde(default = "default_oidc_prefix")]
    pub auth_oidc_username_prefix: String,
    /// Claim carrying group membership.
    #[serde(default = "default_groups_claim")]
    pub auth_oidc_groups_claim: String,
    /// Prepended to every mapped group. Empty = no prefix.
    #[serde(default = "default_oidc_prefix")]
    pub auth_oidc_groups_prefix: String,
    /// Scopes the SPA requests. `offline_access` is what makes most IdPs
    /// issue a refresh token; drop it if yours rejects unknown scopes.
    #[serde(default = "default_oidc_scopes")]
    pub auth_oidc_scopes: String,
    /// Where cluster specs live: "configmap" (standalone, default) or "crd"
    /// (operator mode: PostgresCluster resources are the truth).
    #[serde(default = "default_spec_backend")]
    pub spec_backend: String,
    /// Operator mode only: watch and manage PostgresClusters in every
    /// namespace (needs ClusterRole RBAC). Invalid with the configmap
    /// backend — the server refuses to start.
    #[serde(default)]
    pub all_namespaces: bool,
}

fn default_spec_backend() -> String {
    "configmap".into()
}

fn default_username_claim() -> String {
    "sub".into()
}
fn default_groups_claim() -> String {
    "groups".into()
}
/// Matches Kubernetes' own convention for externally-authenticated subjects,
/// and keeps IdP identities from colliding with ServiceAccount names.
fn default_oidc_prefix() -> String {
    "oidc:".into()
}
fn default_oidc_scopes() -> String {
    "openid profile email offline_access".into()
}

fn default_agent_image() -> String {
    "pglord-agent:dev".into()
}

fn default_http_addr() -> SocketAddr {
    "0.0.0.0:8080".parse().expect("valid literal")
}
fn default_log_level() -> String {
    "info".into()
}
fn default_shutdown_timeout_secs() -> u64 {
    25
}
fn default_empty() -> String {
    String::new()
}

impl Config {
    pub fn load() -> anyhow::Result<Config> {
        let cfg: Config = Figment::new()
            .merge(Toml::file("config.toml"))
            .merge(Env::prefixed("PGLORD_"))
            .extract()?;
        // Fail at startup, not at first login: an issuer without a client ID
        // leaves the UI unable to run the OIDC flow at all.
        if !cfg.auth_oidc_issuer.is_empty() && cfg.auth_oidc_client_id.is_empty() {
            anyhow::bail!("PGLORD_AUTH_OIDC_ISSUER is set but PGLORD_AUTH_OIDC_CLIENT_ID is empty");
        }
        // Authorizing a credential kind that cannot be issued is a
        // misconfiguration, not a mode.
        if cfg.auth_oidc_rbac && cfg.auth_oidc_issuer.is_empty() {
            anyhow::bail!("PGLORD_AUTH_OIDC_RBAC is set but PGLORD_AUTH_OIDC_ISSUER is empty");
        }
        Ok(cfg)
    }

    pub fn shutdown_timeout(&self) -> Duration {
        Duration::from_secs(self.shutdown_timeout_secs)
    }

    pub fn cors_origins(&self) -> Vec<HeaderValue> {
        self.cors_allowed_origins
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect()
    }

    /// Static bearer tokens, split and trimmed; empty vec = static mode off.
    pub fn auth_token_list(&self) -> Vec<String> {
        self.auth_tokens
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    }

    /// Expected `aud` claim; falls back to the client ID when unset.
    pub fn oidc_audience(&self) -> &str {
        if self.auth_oidc_audience.is_empty() {
            &self.auth_oidc_client_id
        } else {
            &self.auth_oidc_audience
        }
    }

    /// True when at least one auth mode is configured. False means the API
    /// runs open (back-compat) and main() logs a loud warning.
    pub fn auth_enabled(&self) -> bool {
        !self.auth_token_list().is_empty()
            || !self.auth_oidc_issuer.is_empty()
            || self.auth_kubernetes
    }

    /// Full literal config for tests; new fields must be added here so existing
    /// tests never break (`Config { x: y, ..Config::test_default() }`).
    pub fn test_default() -> Config {
        Config {
            http_addr: "127.0.0.1:0".parse().expect("valid literal"),
            log_level: "info".into(),
            shutdown_timeout_secs: 1,
            cors_allowed_origins: String::new(),
            default_agent_image: default_agent_image(),
            auth_tokens: String::new(),
            auth_oidc_issuer: String::new(),
            auth_oidc_client_id: String::new(),
            auth_oidc_audience: String::new(),
            auth_kubernetes: false,
            auth_oidc_rbac: false,
            auth_oidc_username_claim: default_username_claim(),
            auth_oidc_username_prefix: default_oidc_prefix(),
            auth_oidc_groups_claim: default_groups_claim(),
            auth_oidc_groups_prefix: default_oidc_prefix(),
            auth_oidc_scopes: default_oidc_scopes(),
            spec_backend: default_spec_backend(),
            all_namespaces: false,
        }
    }
}

#[cfg(test)]
mod tests {
    // figment::Jail closures return figment's large Result type; harmless in tests.
    #![allow(clippy::result_large_err)]
    use super::*;

    #[test]
    fn defaults_load_without_env_or_file() {
        figment::Jail::expect_with(|_jail| {
            let cfg = Config::load().expect("defaults must load");
            assert_eq!(cfg.http_addr.to_string(), "0.0.0.0:8080");
            assert_eq!(cfg.log_level, "info");
            assert_eq!(cfg.shutdown_timeout().as_secs(), 25);
            assert!(cfg.cors_origins().is_empty());
            Ok(())
        });
    }

    #[test]
    fn kubernetes_auth_flag_enables_auth() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("PGLORD_AUTH_KUBERNETES", "true");
            let cfg = Config::load().expect("load");
            assert!(cfg.auth_kubernetes);
            assert!(cfg.auth_enabled());
            Ok(())
        });
    }

    #[test]
    fn all_namespaces_parses_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("PGLORD_SPEC_BACKEND", "crd");
            jail.set_env("PGLORD_ALL_NAMESPACES", "true");
            let cfg = Config::load().expect("load");
            assert!(cfg.all_namespaces);
            assert_eq!(cfg.spec_backend, "crd");
            Ok(())
        });
    }

    #[test]
    fn env_overrides_defaults() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("PGLORD_HTTP_ADDR", "127.0.0.1:9999");
            jail.set_env("PGLORD_CORS_ALLOWED_ORIGINS", "http://a.dev, http://b.dev");
            let cfg = Config::load().expect("env override must load");
            assert_eq!(cfg.http_addr.to_string(), "127.0.0.1:9999");
            assert_eq!(cfg.cors_origins().len(), 2);
            Ok(())
        });
    }

    #[test]
    fn auth_defaults_off() {
        figment::Jail::expect_with(|_jail| {
            let cfg = Config::load().expect("defaults");
            assert!(cfg.auth_token_list().is_empty());
            assert!(!cfg.auth_enabled());
            Ok(())
        });
    }

    #[test]
    fn auth_tokens_parse_comma_separated() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("PGLORD_AUTH_TOKENS", "tok-a, tok-b,");
            let cfg = Config::load().expect("load");
            assert_eq!(cfg.auth_token_list(), vec!["tok-a", "tok-b"]);
            assert!(cfg.auth_enabled());
            Ok(())
        });
    }

    #[test]
    fn oidc_issuer_without_client_id_fails_at_startup() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("PGLORD_AUTH_OIDC_ISSUER", "https://idp.example.com");
            assert!(Config::load().is_err());
            Ok(())
        });
    }

    #[test]
    fn oidc_audience_defaults_to_client_id() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("PGLORD_AUTH_OIDC_ISSUER", "https://idp.example.com");
            jail.set_env("PGLORD_AUTH_OIDC_CLIENT_ID", "pglord");
            let cfg = Config::load().expect("load");
            assert_eq!(cfg.oidc_audience(), "pglord");
            assert!(cfg.auth_enabled());
            Ok(())
        });
    }

    #[test]
    fn oidc_rbac_defaults_off_with_documented_claim_defaults() {
        figment::Jail::expect_with(|_jail| {
            let cfg = Config::load().expect("defaults");
            assert!(!cfg.auth_oidc_rbac);
            assert_eq!(cfg.auth_oidc_username_claim, "sub");
            assert_eq!(cfg.auth_oidc_username_prefix, "oidc:");
            assert_eq!(cfg.auth_oidc_groups_claim, "groups");
            assert_eq!(cfg.auth_oidc_groups_prefix, "oidc:");
            assert_eq!(cfg.auth_oidc_scopes, "openid profile email offline_access");
            Ok(())
        });
    }

    #[test]
    fn oidc_rbac_without_issuer_fails_at_startup() {
        // RBAC with nothing to authorize is a misconfiguration, not a mode.
        figment::Jail::expect_with(|jail| {
            jail.set_env("PGLORD_AUTH_OIDC_RBAC", "true");
            assert!(Config::load().is_err());
            Ok(())
        });
    }

    #[test]
    fn oidc_prefixes_may_be_emptied() {
        // An operator mirroring an apiserver that sets no prefix must be able
        // to ask for none, which has to be distinguishable from "unset".
        figment::Jail::expect_with(|jail| {
            jail.set_env("PGLORD_AUTH_OIDC_ISSUER", "https://idp.example.com");
            jail.set_env("PGLORD_AUTH_OIDC_CLIENT_ID", "pglord");
            jail.set_env("PGLORD_AUTH_OIDC_RBAC", "true");
            jail.set_env("PGLORD_AUTH_OIDC_USERNAME_PREFIX", "");
            jail.set_env("PGLORD_AUTH_OIDC_GROUPS_PREFIX", "");
            jail.set_env("PGLORD_AUTH_OIDC_USERNAME_CLAIM", "email");
            let cfg = Config::load().expect("load");
            assert!(cfg.auth_oidc_rbac);
            assert_eq!(cfg.auth_oidc_username_prefix, "");
            assert_eq!(cfg.auth_oidc_groups_prefix, "");
            assert_eq!(cfg.auth_oidc_username_claim, "email");
            Ok(())
        });
    }

    #[test]
    fn the_default_agent_image_comes_from_the_environment() {
        // This is what the Helm chart's `agentImage` sets. Without it every
        // cluster created on a real deployment lands in ImagePullBackOff,
        // because the compile-time default only exists on a dev machine.
        figment::Jail::expect_with(|_jail| {
            assert_eq!(
                Config::load().expect("defaults").default_agent_image,
                "pglord-agent:dev"
            );
            Ok(())
        });
        figment::Jail::expect_with(|jail| {
            jail.set_env("PGLORD_DEFAULT_AGENT_IMAGE", "reg.example.com/agent:1.2.3");
            assert_eq!(
                Config::load().expect("env").default_agent_image,
                "reg.example.com/agent:1.2.3"
            );
            Ok(())
        });
    }
}
