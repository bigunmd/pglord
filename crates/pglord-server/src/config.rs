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
    /// Where cluster specs live: "configmap" (standalone, default) or "crd"
    /// (operator mode: PostgresCluster resources are the truth).
    #[serde(default = "default_spec_backend")]
    pub spec_backend: String,
}

fn default_spec_backend() -> String {
    "configmap".into()
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
        !self.auth_token_list().is_empty() || !self.auth_oidc_issuer.is_empty()
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
            spec_backend: default_spec_backend(),
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
