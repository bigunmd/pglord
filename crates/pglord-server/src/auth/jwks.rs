//! OIDC resource-server validation: incoming access tokens are JWTs checked
//! against the issuer's published JWKS. Discovery and key fetch are lazy —
//! never at startup — so a slow or absent IdP cannot block the server; a
//! fetch failure just rejects the token (401), it is never a 500.

use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::Mutex;

/// Floor between JWKS refetches on unknown-`kid` tokens, so a stream of junk
/// tokens cannot hammer the IdP.
const REFETCH_MIN_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
struct DiscoveryDoc {
    jwks_uri: String,
}

pub struct OidcValidator {
    issuer: String,
    audience: String,
    client_id: String,
    http: reqwest::Client,
    refetch_min: Duration,
    cache: Mutex<KeyCache>,
}

#[derive(Default)]
struct KeyCache {
    jwks: Option<JwkSet>,
    last_fetch: Option<Instant>,
}

impl OidcValidator {
    pub fn new(issuer: String, client_id: String, audience: String, http: reqwest::Client) -> Self {
        OidcValidator {
            issuer,
            audience,
            client_id,
            http,
            refetch_min: REFETCH_MIN_INTERVAL,
            cache: Mutex::new(KeyCache::default()),
        }
    }

    #[cfg(test)]
    fn with_refetch_min(mut self, refetch_min: Duration) -> Self {
        self.refetch_min = refetch_min;
        self
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// None = rejected. Some(claims) = a valid token; the caller may map the
    /// claims to an authorization subject.
    pub async fn validate(&self, token: &str) -> Option<serde_json::Value> {
        match self.validate_inner(token).await {
            Ok(claims) => Some(claims),
            Err(e) => {
                tracing::warn!(error = %e, "OIDC token rejected");
                None
            }
        }
    }

    async fn validate_inner(&self, token: &str) -> anyhow::Result<serde_json::Value> {
        let header = decode_header(token)?;
        // Asymmetric signatures only: accepting an HMAC alg here would let a
        // client sign tokens with the *public* key material (alg confusion).
        let alg = match header.alg {
            Algorithm::RS256
            | Algorithm::RS384
            | Algorithm::RS512
            | Algorithm::PS256
            | Algorithm::PS384
            | Algorithm::PS512
            | Algorithm::ES256
            | Algorithm::ES384 => header.alg,
            other => anyhow::bail!("token algorithm {other:?} is not accepted"),
        };
        let kid = header
            .kid
            .ok_or_else(|| anyhow::anyhow!("token has no kid header"))?;
        let jwk = self
            .key_for(&kid)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no JWKS key matches kid {kid:?}"))?;
        let key = DecodingKey::from_jwk(&jwk)?;
        let mut validation = Validation::new(alg);
        validation.set_audience(&[self.audience.as_str()]);
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_required_spec_claims(&["exp", "aud", "iss"]);
        Ok(decode::<serde_json::Value>(token, &key, &validation)?.claims)
    }

    /// Cached key lookup; on a miss, refetch the JWKS (rate-limited) so
    /// IdP-side key rotation is picked up without a restart.
    async fn key_for(&self, kid: &str) -> anyhow::Result<Option<Jwk>> {
        let mut cache = self.cache.lock().await;
        if let Some(found) = cache.jwks.as_ref().and_then(|j| j.find(kid)) {
            return Ok(Some(found.clone()));
        }
        let due = cache
            .last_fetch
            .is_none_or(|at| at.elapsed() >= self.refetch_min);
        if !due {
            return Ok(None);
        }
        cache.last_fetch = Some(Instant::now());
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            self.issuer.trim_end_matches('/')
        );
        let discovery: DiscoveryDoc = self
            .http
            .get(discovery_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let jwks: JwkSet = self
            .http
            .get(discovery.jwks_uri)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        cache.jwks = Some(jwks);
        Ok(cache.jwks.as_ref().and_then(|j| j.find(kid)).cloned())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use axum::Router;
    use axum::extract::State;
    use axum::routing::get;
    use jsonwebtoken::{EncodingKey, Header, encode};

    use super::*;

    /// Test-only RSA keypair (generated for this test file, secret to no one).
    const TEST_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDjE5Gzj3fcvJra
snlO7ldahdl3SV0wzYsWeZamYoppCqTyVG5mqD4DlMew3HHj7LF+5NT7WWhAB3gd
erUgbhTuQUvyDrdEVwbwRr88kpsNj8gUDH01TuRlsbNqwG84GLvFk0T2ffY3O6Yt
y5/yC3HR0n1B7yslQB/5ICm+ZpXhhbi1cxBaxdtrDqwH1EiCar9IdkjAgKe9UsUh
KjaOIkS2OzLgKlxMuEFnPwSGPNlVQRGI1iJVxdyzFYuOeEH8Q+Dbov/6ehJbwG8D
wxdFvrpUXnrbTMWTRsPjT8iL8P1n3C2fkA3oKogo2lM55EFEeEPn7bu9RWahYe5w
6K/tB5w1AgMBAAECggEACDf5GgqjR/X7lNbI1TaDKt+VU1/wY4fM9O9JqJQAdnTQ
K4UqycRlGjQUVi5kYN98PR+DJeXChC7BBjAexiG12Dm8yfIGaA7OTXwCB+s2eXMu
xqr9DSTh11xrJf35CIq8oRYHdPRyKOWbyiSIkXYglHuN1QkXAq4jehzT5fRIkHT5
Gk4u2xIqx/zGCsLRNGdK3hpMqQPdt7NrlqLaQ6fUMo7BFZ6NXcSHzrt94DezmVrY
vVaJHwuzq/JkAwCpv8tSXI39gaRnVk+yfRVG37b2Pc/ZPwSkOdj9LjfyIj+fNtfh
6BbrrwurWo8jPcNNtddszUOuEr+lulGFth0ce6DNgQKBgQDzVPmsBWddb8NsSOD/
Jj6nmNLBRdFsNbyxQaiMpz1jn2XKitJsPZqkiyZXFdSg9DjQJb5vX1rzuPQgt+XH
hr95i1jZ0jbnT/SPSZPglp0WaO8ePGJCSY61H7rLegFmJgFqxMJtw+K7dymEFCvs
vTKeHytGnmN3HJpQQJ+8C/2TDQKBgQDu5fKTQPf6guWUaPLbyqJ1NxFQuhaL34fo
exaBDRb5AcsV1nZENOadzX5+ajFp6S++/ib5l1qkzk5RpiYKKreLKIj0jRMktY9Z
YiRs+MEozP1+annrfdLtm75k36s5T8Q7TYwH+FEJwWURGcoWPVQKS/IELxoBcCOR
RtxQnY4DyQKBgQClUBm/UHyE8iUmL+0L6wOuARlNzQ0WIvvqwNytkgEW6XfSPrXu
a3vgRdjqWR53qvPeFYZnS7US15ijkcw93cG/5XebpEFArQVNQ4N/lVNroiaxt/xZ
ZHfbfY7+JglFSJS14K7mu6oSCkbeQIYG9BKrcimIdJrGlVwRIZZjvxGrEQKBgQDR
n5AkyeNOO8F7YkOHvNykgbd94GucPH4i7q6menfn8zziYpbP7qNVYFVYU5IcXvL4
YkaMx9RPvH9YgbPrQ+ReBSl3fLqJgY7RXZ6cyAGl7YmZfZCRpIyK1hhQpzq1RVro
tnjjHW7uYnZudPSwYwv1JQKtzS7CiEWNWmJDkpxkkQKBgGxa+kFZyAbB32lCSl9U
UmQP+82fWREHjUug04ev79Xoh1T289eDTBAFUAqc0hUbWdPh83mBzpMiIzWBp2Gc
FcVOMLIvL20YEtArShgIomndTbLWzdS244hhv0FUtp1Dp0tZVVHn/VP9IfzNnIjH
aHCSV9HUW+HVYQd4vniU6qpC
-----END PRIVATE KEY-----";

    const TEST_JWK_N: &str = "4xORs4933Lya2rJ5Tu5XWoXZd0ldMM2LFnmWpmKKaQqk8lRuZqg-A5THsNxx4-yxfuTU-1loQAd4HXq1IG4U7kFL8g63RFcG8Ea_PJKbDY_IFAx9NU7kZbGzasBvOBi7xZNE9n32NzumLcuf8gtx0dJ9Qe8rJUAf-SApvmaV4YW4tXMQWsXbaw6sB9RIgmq_SHZIwICnvVLFISo2jiJEtjsy4CpcTLhBZz8EhjzZVUERiNYiVcXcsxWLjnhB_EPg26L_-noSW8BvA8MXRb66VF5620zFk0bD40_Ii_D9Z9wtn5AN6CqIKNpTOeRBRHhD5-27vUVmoWHucOiv7QecNQ";

    #[derive(Clone)]
    struct MockIdp {
        base: String,
        jwks_fetches: Arc<AtomicUsize>,
        serve_second_key: Arc<std::sync::atomic::AtomicBool>,
    }

    fn jwk_json(kid: &str) -> serde_json::Value {
        serde_json::json!({
            "kty": "RSA", "alg": "RS256", "use": "sig",
            "kid": kid, "n": TEST_JWK_N, "e": "AQAB"
        })
    }

    async fn spawn_idp() -> MockIdp {
        let fetches = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let idp = MockIdp {
            base: base.clone(),
            jwks_fetches: fetches.clone(),
            serve_second_key: second.clone(),
        };
        let state = idp.clone();
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(|State(s): State<MockIdp>| async move {
                    axum::Json(serde_json::json!({"jwks_uri": format!("{}/jwks", s.base)}))
                }),
            )
            .route(
                "/jwks",
                get(|State(s): State<MockIdp>| async move {
                    s.jwks_fetches.fetch_add(1, Ordering::SeqCst);
                    let mut keys = vec![jwk_json("test-key")];
                    if s.serve_second_key.load(Ordering::SeqCst) {
                        keys.push(jwk_json("rotated-key"));
                    }
                    axum::Json(serde_json::json!({"keys": keys}))
                }),
            )
            .with_state(state);
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        idp
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs()
    }

    fn mint(kid: Option<&str>, iss: &str, aud: &str, exp: u64) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = kid.map(String::from);
        let claims = serde_json::json!({"iss": iss, "aud": aud, "exp": exp, "sub": "alice"});
        let key = EncodingKey::from_rsa_pem(TEST_PEM.as_bytes()).expect("test key");
        encode(&header, &claims, &key).expect("encode")
    }

    fn validator(idp: &MockIdp) -> OidcValidator {
        OidcValidator::new(
            idp.base.clone(),
            "pglord".into(),
            "pglord".into(),
            reqwest::Client::new(),
        )
    }

    #[tokio::test]
    async fn valid_token_is_accepted_and_keys_are_cached() {
        let idp = spawn_idp().await;
        let v = validator(&idp);
        let token = mint(Some("test-key"), &idp.base, "pglord", now() + 3600);
        assert!(v.validate(&token).await.is_some());
        assert!(v.validate(&token).await.is_some());
        assert_eq!(idp.jwks_fetches.load(Ordering::SeqCst), 1, "cache hit");
    }

    #[tokio::test]
    async fn valid_token_returns_its_claims() {
        // The claims are what the RBAC path maps to a Kubernetes subject, so
        // they have to survive validation rather than be decoded and dropped.
        let idp = spawn_idp().await;
        let v = validator(&idp);
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-key".into());
        let claims = serde_json::json!({
            "iss": idp.base, "aud": "pglord", "exp": now() + 3600,
            "sub": "alice", "email": "alice@corp.com",
            "groups": ["platform-team", "sre"],
        });
        let key = EncodingKey::from_rsa_pem(TEST_PEM.as_bytes()).expect("test key");
        let token = encode(&header, &claims, &key).expect("encode");
        let got = v.validate(&token).await.expect("accepted");
        assert_eq!(got["email"], "alice@corp.com");
        assert_eq!(got["groups"][1], "sre");
    }

    #[tokio::test]
    async fn expired_token_is_rejected() {
        let idp = spawn_idp().await;
        let v = validator(&idp);
        let token = mint(Some("test-key"), &idp.base, "pglord", now() - 3600);
        assert!(v.validate(&token).await.is_none());
    }

    #[tokio::test]
    async fn wrong_audience_is_rejected() {
        let idp = spawn_idp().await;
        let v = validator(&idp);
        let token = mint(Some("test-key"), &idp.base, "other-app", now() + 3600);
        assert!(v.validate(&token).await.is_none());
    }

    #[tokio::test]
    async fn wrong_issuer_is_rejected() {
        let idp = spawn_idp().await;
        let v = validator(&idp);
        let token = mint(
            Some("test-key"),
            "https://evil.example.com",
            "pglord",
            now() + 3600,
        );
        assert!(v.validate(&token).await.is_none());
    }

    #[tokio::test]
    async fn hmac_alg_confusion_is_rejected() {
        let idp = spawn_idp().await;
        let v = validator(&idp);
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("test-key".into());
        let claims = serde_json::json!({"iss": idp.base, "aud": "pglord", "exp": now() + 3600});
        let key = EncodingKey::from_secret(b"guessable");
        let token = encode(&header, &claims, &key).expect("encode");
        assert!(v.validate(&token).await.is_none());
        assert_eq!(
            idp.jwks_fetches.load(Ordering::SeqCst),
            0,
            "rejected before any fetch"
        );
    }

    #[tokio::test]
    async fn token_without_kid_is_rejected() {
        let idp = spawn_idp().await;
        let v = validator(&idp);
        let token = mint(None, &idp.base, "pglord", now() + 3600);
        assert!(v.validate(&token).await.is_none());
    }

    #[tokio::test]
    async fn unknown_kid_does_not_refetch_within_rate_limit() {
        let idp = spawn_idp().await;
        let v = validator(&idp);
        let good = mint(Some("test-key"), &idp.base, "pglord", now() + 3600);
        assert!(v.validate(&good).await.is_some());
        let unknown = mint(Some("rotated-key"), &idp.base, "pglord", now() + 3600);
        assert!(v.validate(&unknown).await.is_none());
        assert_eq!(
            idp.jwks_fetches.load(Ordering::SeqCst),
            1,
            "refetch rate-limited"
        );
    }

    #[tokio::test]
    async fn rotated_key_is_picked_up_by_refetch() {
        let idp = spawn_idp().await;
        let v = validator(&idp).with_refetch_min(Duration::ZERO);
        let good = mint(Some("test-key"), &idp.base, "pglord", now() + 3600);
        assert!(v.validate(&good).await.is_some());
        idp.serve_second_key.store(true, Ordering::SeqCst);
        let rotated = mint(Some("rotated-key"), &idp.base, "pglord", now() + 3600);
        assert!(v.validate(&rotated).await.is_some());
        assert_eq!(idp.jwks_fetches.load(Ordering::SeqCst), 2, "one refetch");
    }

    #[tokio::test]
    async fn unreachable_idp_rejects_instead_of_erroring() {
        let v = OidcValidator::new(
            "http://127.0.0.1:1".into(),
            "pglord".into(),
            "pglord".into(),
            reqwest::Client::new(),
        );
        let token = mint(
            Some("test-key"),
            "http://127.0.0.1:1",
            "pglord",
            now() + 3600,
        );
        assert!(v.validate(&token).await.is_none());
    }
}
