//! Client access rules (pg_hba.conf) expressed as structured, validated data.
//!
//! pg_hba is first-match-wins, so placement is the safety model: the managed
//! prefix (local/loopback access, the replication rule) always comes first and
//! the scram catch-all always comes last. User rules sit in between — they can
//! tighten access with explicit `reject` rules but can never break HA, lock
//! out the agent, or silently drop all auth.

use serde::{Deserialize, Serialize};

/// Generous ceiling; real deployments have a handful of rules.
pub const MAX_RULES: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum HbaConn {
    Host,
    HostSsl,
    HostNoSsl,
}

impl HbaConn {
    fn as_str(self) -> &'static str {
        match self {
            HbaConn::Host => "host",
            HbaConn::HostSsl => "hostssl",
            HbaConn::HostNoSsl => "hostnossl",
        }
    }
}

/// Deliberately small: ldap/radius need auth options this schema does not
/// carry, md5 is deprecated. Growing the list later is backwards-compatible;
/// shrinking it is not. `cert` authenticates by client certificate (CN =
/// role name, verify-full); it is only meaningful over TLS, so it is valid
/// only on `hostssl` rules and only when the cluster has TLS enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum HbaMethod {
    #[serde(rename = "scram-sha-256")]
    ScramSha256,
    #[serde(rename = "reject")]
    Reject,
    #[serde(rename = "trust")]
    Trust,
    #[serde(rename = "cert")]
    Cert,
}

impl HbaMethod {
    fn as_str(self) -> &'static str {
        match self {
            HbaMethod::ScramSha256 => "scram-sha-256",
            HbaMethod::Reject => "reject",
            HbaMethod::Trust => "trust",
            HbaMethod::Cert => "cert",
        }
    }
}

/// One user-supplied pg_hba rule. `local` is not expressible: the managed
/// prefix owns socket access, and a user local rule after `local all all peer`
/// could never match anyway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HbaRule {
    pub conn: HbaConn,
    /// "all", "replication", or a single database name.
    pub database: String,
    /// "all" or a single role name.
    pub user: String,
    /// CIDR ("10.0.0.0/8", "::1/128") or all | samehost | samenet.
    pub address: String,
    pub method: HbaMethod,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HbaError {
    #[error("too many pg_hba rules: {0} (max {MAX_RULES})")]
    TooManyRules(usize),
    #[error(
        "invalid pg_hba {field} '{value}': must be a keyword or an identifier \
         (letter or _, then letters, digits, _ or $; at most 63 bytes)"
    )]
    BadToken { field: &'static str, value: String },
    #[error(
        "invalid pg_hba address '{0}': must be a CIDR like '10.0.0.0/8' or '::1/128', \
         or one of all, samehost, samenet (hostnames are not supported)"
    )]
    BadAddress(String),
    #[error("pg_hba method 'cert' requires connection type 'hostssl'")]
    CertRequiresHostSsl,
}

pub fn validate_rules(rules: &[HbaRule]) -> Result<(), HbaError> {
    if rules.len() > MAX_RULES {
        return Err(HbaError::TooManyRules(rules.len()));
    }
    for rule in rules {
        check_token("database", &rule.database, &["all", "replication"])?;
        check_token("user", &rule.user, &["all"])?;
        check_address(&rule.address)?;
        // cert auth happens during the TLS handshake; on a plain `host` line
        // it could never succeed and would read as a silent lockout.
        if rule.method == HbaMethod::Cert && rule.conn != HbaConn::HostSsl {
            return Err(HbaError::CertRequiresHostSsl);
        }
    }
    Ok(())
}

fn check_token(field: &'static str, value: &str, keywords: &[&str]) -> Result<(), HbaError> {
    if keywords.contains(&value) {
        return Ok(());
    }
    let mut chars = value.chars();
    let head_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let ok = head_ok
        && value.len() <= 63
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if ok {
        Ok(())
    } else {
        Err(HbaError::BadToken {
            field,
            value: value.to_string(),
        })
    }
}

fn check_address(value: &str) -> Result<(), HbaError> {
    if matches!(value, "all" | "samehost" | "samenet") {
        return Ok(());
    }
    let bad = || HbaError::BadAddress(value.to_string());
    let (addr, prefix) = value.split_once('/').ok_or_else(bad)?;
    let ip: std::net::IpAddr = addr.parse().map_err(|_| bad())?;
    let bits: u8 = prefix.parse().map_err(|_| bad())?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    if bits > max { Err(bad()) } else { Ok(()) }
}

/// The entire pg_hba.conf as a pure function of the spec. The prefix mirrors
/// what initdb produced before this feature (`--auth-local peer --auth-host
/// scram-sha-256`, or `-A trust` without a password), so upgrading rewrites
/// the file without changing its effective rules.
pub fn render(rules: &[HbaRule], repl_user: &str, scram: bool) -> String {
    let mut out =
        String::from("# Managed by pglord - do not edit; changes come from the cluster spec.\n");
    if scram {
        out.push_str("local all all peer\n");
        out.push_str("host all all 127.0.0.1/32 scram-sha-256\n");
        out.push_str("host all all ::1/128 scram-sha-256\n");
        out.push_str("local replication all peer\n");
        out.push_str(&format!(
            "host replication {repl_user} 0.0.0.0/0 scram-sha-256\n"
        ));
    } else {
        out.push_str("# INSECURE trust mode: no password configured.\n");
        out.push_str("local all all trust\n");
        out.push_str("host all all 127.0.0.1/32 trust\n");
        out.push_str("host all all ::1/128 trust\n");
        out.push_str("local replication all trust\n");
        out.push_str("host replication all 0.0.0.0/0 trust\n");
    }
    if !rules.is_empty() {
        out.push_str("\n# User rules (cluster spec pg_hba)\n");
        for r in rules {
            out.push_str(&format!(
                "{} {} {} {} {}\n",
                r.conn.as_str(),
                r.database,
                r.user,
                r.address,
                r.method.as_str()
            ));
        }
    }
    out.push_str("\n# Managed catch-all\n");
    if scram {
        out.push_str("host all all 0.0.0.0/0 scram-sha-256\n");
    } else {
        out.push_str("host all all 0.0.0.0/0 trust\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(conn: HbaConn, db: &str, user: &str, addr: &str, method: HbaMethod) -> HbaRule {
        HbaRule {
            conn,
            database: db.into(),
            user: user.into(),
            address: addr.into(),
            method,
        }
    }

    #[test]
    fn accepts_valid_rules() {
        let rules = vec![
            rule(
                HbaConn::Host,
                "all",
                "bad_actor",
                "0.0.0.0/0",
                HbaMethod::Reject,
            ),
            rule(
                HbaConn::HostSsl,
                "app",
                "app_rw",
                "10.0.0.0/8",
                HbaMethod::ScramSha256,
            ),
            rule(
                HbaConn::HostNoSsl,
                "replication",
                "all",
                "samenet",
                HbaMethod::Trust,
            ),
            rule(
                HbaConn::Host,
                "db1",
                "u$1",
                "::1/128",
                HbaMethod::ScramSha256,
            ),
            rule(
                HbaConn::Host,
                "_x",
                "all",
                "samehost",
                HbaMethod::ScramSha256,
            ),
        ];
        assert_eq!(validate_rules(&rules), Ok(()));
    }

    #[test]
    fn rejects_bad_tokens() {
        let long = "x".repeat(64);
        for (db, user) in [
            ("my db", "all"),       // whitespace
            ("db,other", "all"),    // comma (one rule, one database)
            ("\"quoted\"", "all"),  // quotes
            ("1db", "all"),         // digit start
            ("", "all"),            // empty
            ("all", long.as_str()), // too long
        ] {
            let r = vec![rule(HbaConn::Host, db, user, "all", HbaMethod::Reject)];
            assert!(
                matches!(validate_rules(&r), Err(HbaError::BadToken { .. })),
                "expected BadToken for db='{db}' user='{user}'"
            );
        }
    }

    #[test]
    fn rejects_bad_addresses() {
        for addr in [
            "10.0.0.0",        // no prefix
            "10.0.0.0/33",     // prefix too wide for v4
            "::1/129",         // prefix too wide for v6
            "example.com",     // hostname
            "10.0.0.0/-1",     // negative
            "10.0.0.0/8/8",    // double slash
            "0.0.0.0/0 extra", // trailing garbage
        ] {
            let r = vec![rule(HbaConn::Host, "all", "all", addr, HbaMethod::Reject)];
            assert!(
                matches!(validate_rules(&r), Err(HbaError::BadAddress(_))),
                "expected BadAddress for '{addr}'"
            );
        }
    }

    #[test]
    fn cert_is_valid_only_on_hostssl_rules() {
        let ok = vec![rule(
            HbaConn::HostSsl,
            "app",
            "app_rw",
            "10.0.0.0/8",
            HbaMethod::Cert,
        )];
        assert_eq!(validate_rules(&ok), Ok(()));
        for conn in [HbaConn::Host, HbaConn::HostNoSsl] {
            let bad = vec![rule(conn, "all", "all", "all", HbaMethod::Cert)];
            assert_eq!(validate_rules(&bad), Err(HbaError::CertRequiresHostSsl));
        }
    }

    #[test]
    fn cert_renders_with_its_pg_hba_spelling() {
        let rules = vec![rule(
            HbaConn::HostSsl,
            "app",
            "app_rw",
            "10.0.0.0/8",
            HbaMethod::Cert,
        )];
        let out = render(&rules, "replicator", true);
        assert!(out.contains("hostssl app app_rw 10.0.0.0/8 cert\n"));
        let parsed: HbaRule = serde_json::from_str(
            r#"{"conn":"hostssl","database":"app","user":"app_rw",
                "address":"10.0.0.0/8","method":"cert"}"#,
        )
        .expect("deserialize");
        assert_eq!(parsed.method, HbaMethod::Cert);
    }

    #[test]
    fn rejects_too_many_rules() {
        let rules: Vec<HbaRule> = (0..101)
            .map(|_| rule(HbaConn::Host, "all", "all", "all", HbaMethod::Reject))
            .collect();
        assert_eq!(validate_rules(&rules), Err(HbaError::TooManyRules(101)));
    }

    #[test]
    fn serde_uses_pg_hba_spellings_and_rejects_unknown() {
        let json = r#"{"conn":"hostssl","database":"app","user":"app_rw",
                       "address":"10.0.0.0/8","method":"scram-sha-256"}"#;
        let r: HbaRule = serde_json::from_str(json).expect("deserialize");
        assert_eq!(r.conn, HbaConn::HostSsl);
        assert_eq!(r.method, HbaMethod::ScramSha256);
        // Unknown method must fail at the serde layer, before validation.
        assert!(
            serde_json::from_str::<HbaRule>(
                r#"{"conn":"host","database":"all","user":"all","address":"all","method":"md5"}"#
            )
            .is_err()
        );
        // `local` is not a representable connection type.
        assert!(
            serde_json::from_str::<HbaRule>(
                r#"{"conn":"local","database":"all","user":"all","address":"all","method":"trust"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn render_scram_no_user_rules() {
        let out = render(&[], "replicator", true);
        assert_eq!(
            out,
            "# Managed by pglord - do not edit; changes come from the cluster spec.\n\
             local all all peer\n\
             host all all 127.0.0.1/32 scram-sha-256\n\
             host all all ::1/128 scram-sha-256\n\
             local replication all peer\n\
             host replication replicator 0.0.0.0/0 scram-sha-256\n\
             \n\
             # Managed catch-all\n\
             host all all 0.0.0.0/0 scram-sha-256\n"
        );
    }

    #[test]
    fn render_places_user_rules_between_prefix_and_catch_all() {
        let rules = vec![
            rule(
                HbaConn::Host,
                "all",
                "bad_actor",
                "0.0.0.0/0",
                HbaMethod::Reject,
            ),
            rule(
                HbaConn::HostSsl,
                "app",
                "app_rw",
                "10.0.0.0/8",
                HbaMethod::ScramSha256,
            ),
        ];
        let out = render(&rules, "replicator", true);
        let repl = out.find("host replication replicator").expect("repl rule");
        let reject = out
            .find("host all bad_actor 0.0.0.0/0 reject")
            .expect("user rule 1");
        let app = out
            .find("hostssl app app_rw 10.0.0.0/8 scram-sha-256")
            .expect("user rule 2");
        let catch_all = out
            .rfind("host all all 0.0.0.0/0 scram-sha-256")
            .expect("catch-all");
        assert!(repl < reject && reject < app && app < catch_all);
        assert!(out.ends_with("host all all 0.0.0.0/0 scram-sha-256\n"));
    }

    #[test]
    fn render_trust_mode_matches_dev_bootstrap() {
        let out = render(&[], "replicator", false);
        assert!(out.contains("INSECURE"));
        assert!(out.contains("local all all trust\n"));
        assert!(out.contains("host replication all 0.0.0.0/0 trust\n"));
        assert!(out.ends_with("host all all 0.0.0.0/0 trust\n"));
        assert!(!out.contains("scram-sha-256"));
        assert!(!out.contains("peer"));
    }
}
