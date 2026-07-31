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
        Ok(Figment::new()
            .merge(Toml::file("config.toml"))
            .merge(Env::prefixed("PGLORD_"))
            .extract()?)
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

    /// Full literal config for tests; new fields must be added here so existing
    /// tests never break (`Config { x: y, ..Config::test_default() }`).
    pub fn test_default() -> Config {
        Config {
            http_addr: "127.0.0.1:0".parse().expect("valid literal"),
            log_level: "info".into(),
            shutdown_timeout_secs: 1,
            cors_allowed_origins: String::new(),
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
}
