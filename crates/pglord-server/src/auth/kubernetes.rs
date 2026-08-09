//! Kubernetes-native auth: TokenReview authenticates a presented bearer
//! token, SubjectAccessReview authorizes the request against
//! `pglord.dev/postgresclusters` in the target namespace. Whoever may
//! `kubectl patch pgc` in a namespace may do the same through the API.
//!
//! Verbs are checked on the API's nouns regardless of the storage backend —
//! RBAC does not require the resource to exist.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec,
};
use kube::api::{Api, PostParams};
use sha2::{Digest, Sha256};

const TOKEN_TTL: Duration = Duration::from_secs(60);
const SAR_TTL: Duration = Duration::from_secs(10);

/// Who a request is, in the terms RBAC matches on. Built from a TokenReview
/// on the Kubernetes path and from JWT claims on the OIDC one; a
/// SubjectAccessReview cannot tell the difference, which is the whole point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub user: String,
    pub groups: Vec<String>,
}

/// Turns validated OIDC claims into a `Subject`. Pure: no I/O, no clock.
pub struct SubjectMapping {
    username_claim: String,
    username_prefix: String,
    groups_claim: String,
    groups_prefix: String,
}

impl SubjectMapping {
    pub fn new(cfg: &crate::config::Config) -> SubjectMapping {
        SubjectMapping {
            username_claim: cfg.auth_oidc_username_claim.clone(),
            username_prefix: cfg.auth_oidc_username_prefix.clone(),
            groups_claim: cfg.auth_oidc_groups_claim.clone(),
            groups_prefix: cfg.auth_oidc_groups_prefix.clone(),
        }
    }

    /// None = the token carries no usable identity. Nothing is lowercased or
    /// normalized: RBAC subject matching is byte-exact, so rewriting an
    /// identity here would produce grants the operator never wrote.
    pub fn subject(&self, claims: &serde_json::Value) -> Option<Subject> {
        let name = claims.get(&self.username_claim)?.as_str()?;
        if name.is_empty() {
            return None;
        }
        let raw = claims.get(&self.groups_claim);
        // IdPs disagree on whether a single group is an array or a string.
        let groups = match raw {
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .filter_map(|g| g.as_str())
                .map(|g| format!("{}{g}", self.groups_prefix))
                .collect(),
            Some(serde_json::Value::String(one)) => {
                vec![format!("{}{one}", self.groups_prefix)]
            }
            _ => Vec::new(),
        };
        Some(Subject {
            user: format!("{}{name}", self.username_prefix),
            groups,
        })
    }

    /// Display only — never used for authorization.
    pub fn display_name(&self, claims: &serde_json::Value) -> Option<String> {
        ["name", "preferred_username", "email"]
            .iter()
            .find_map(|c| claims.get(*c).and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty())
            .map(String::from)
    }
}

/// What a request needs, derived purely from method + path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// Authenticated is enough (ping, version, gucs, preflight): these
    /// endpoints expose no cluster data.
    Authenticated,
    /// SAR on postgresclusters with this verb; ns None = cluster-wide.
    Cluster {
        verb: &'static str,
        ns: Option<String>,
    },
}

/// Map a request to its authorization target. The alias middleware has
/// already rewritten legacy paths, so cluster paths have exactly one shape:
/// `/api/v1/namespaces/{ns}/clusters[/{name}[/...]]`.
pub fn authz_target(method: &axum::http::Method, path: &str) -> Access {
    use axum::http::Method;
    if path == "/api/v1/clusters" {
        // The global list spans namespaces: needs cluster-wide list.
        return Access::Cluster {
            verb: "list",
            ns: None,
        };
    }
    let Some(rest) = path.strip_prefix("/api/v1/namespaces/") else {
        return Access::Authenticated;
    };
    let mut parts = rest.split('/');
    let Some(ns) = parts.next().filter(|s| !s.is_empty()) else {
        return Access::Authenticated;
    };
    if parts.next() != Some("clusters") {
        return Access::Authenticated;
    }
    let ns = Some(ns.to_string());
    let name = parts.next().filter(|s| !s.is_empty());
    let tail: Vec<&str> = parts.collect();
    let verb = match (method, name, tail.as_slice()) {
        (&Method::GET, None, _) => "list",
        (&Method::POST, None, _) => "create",
        (&Method::GET, Some(_), _) => "get",
        (&Method::PATCH, Some(_), []) => "patch",
        (&Method::DELETE, Some(_), []) => "delete",
        // A restore creates a new cluster in the namespace.
        (&Method::POST, Some(_), ["restore"]) => "create",
        // Every other day-2 operation mutates the cluster: patch.
        (&Method::POST, Some(_), _) => "patch",
        _ => "get",
    };
    Access::Cluster { verb, ns }
}

pub fn denial_detail(user: &str, access: &Access) -> String {
    match access {
        Access::Authenticated => format!("user '{user}' is not permitted here"),
        Access::Cluster { verb, ns: Some(ns) } => {
            format!("user '{user}' cannot {verb} postgresclusters in namespace '{ns}'")
        }
        Access::Cluster { verb, ns: None } => {
            format!("user '{user}' cannot {verb} postgresclusters cluster-wide")
        }
    }
}

struct AuthEntry {
    subject: Subject,
    expires: Instant,
}

/// (token hash, verb, ns) — one grant per authenticated subject and target.
type GrantKey = (String, &'static str, Option<String>);

pub struct KubernetesAuthorizer {
    client: kube::Client,
    tokens: Mutex<HashMap<String, AuthEntry>>,
    /// Grant -> allowed-until. Only positive results live here; denials are
    /// re-checked every time.
    grants: Mutex<HashMap<GrantKey, Instant>>,
}

/// Raw tokens never sit in a map key.
pub fn token_key(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    format!("{:x}", h.finalize())
}

impl KubernetesAuthorizer {
    pub fn new(client: kube::Client) -> KubernetesAuthorizer {
        KubernetesAuthorizer {
            client,
            tokens: Mutex::new(HashMap::new()),
            grants: Mutex::new(HashMap::new()),
        }
    }

    /// None = rejected. Some(subject) = authenticated (cached ~60s).
    pub async fn authenticate(&self, token: &str) -> Option<Subject> {
        let key = token_key(token);
        if let Ok(cache) = self.tokens.lock()
            && let Some(entry) = cache.get(&key)
            && entry.expires > Instant::now()
        {
            return Some(entry.subject.clone());
        }
        let api: Api<TokenReview> = Api::all(self.client.clone());
        let review = TokenReview {
            spec: TokenReviewSpec {
                token: token.to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let status = api
            .create(&PostParams::default(), &review)
            .await
            .ok()?
            .status?;
        if status.authenticated != Some(true) {
            return None;
        }
        let user_info = status.user?;
        let user = user_info.username.unwrap_or_default();
        if user.is_empty() {
            return None;
        }
        let subject = Subject {
            user,
            groups: user_info.groups.unwrap_or_default(),
        };
        if let Ok(mut cache) = self.tokens.lock() {
            cache.retain(|_, e| e.expires > Instant::now());
            cache.insert(
                key,
                AuthEntry {
                    subject: subject.clone(),
                    expires: Instant::now() + TOKEN_TTL,
                },
            );
        }
        Some(subject)
    }

    /// SAR the mapped target for an already-authenticated subject. The
    /// subject is passed in rather than looked up, so the OIDC path — which
    /// has no TokenReview entry — uses the identical code, and so an expired
    /// cache entry can no longer silently strip the groups from the review.
    /// `cache_key` scopes the grant cache to one credential.
    pub async fn authorize(&self, cache_key: &str, subject: &Subject, access: &Access) -> bool {
        let (verb, ns) = match access {
            Access::Authenticated => return true,
            Access::Cluster { verb, ns } => (*verb, ns.clone()),
        };
        let key = (cache_key.to_string(), verb, ns.clone());
        if let Ok(cache) = self.grants.lock()
            && let Some(until) = cache.get(&key)
            && *until > Instant::now()
        {
            return true;
        }
        let groups = subject.groups.clone();
        let api: Api<SubjectAccessReview> = Api::all(self.client.clone());
        let review = SubjectAccessReview {
            spec: SubjectAccessReviewSpec {
                user: Some(subject.user.clone()),
                groups: (!groups.is_empty()).then_some(groups),
                resource_attributes: Some(ResourceAttributes {
                    group: Some("pglord.dev".to_string()),
                    resource: Some("postgresclusters".to_string()),
                    verb: Some(verb.to_string()),
                    namespace: ns.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let allowed = match api.create(&PostParams::default(), &review).await {
            Ok(resp) => resp.status.map(|s| s.allowed).unwrap_or(false),
            Err(err) => {
                tracing::warn!(%err, "SubjectAccessReview failed; denying");
                false
            }
        };
        if allowed && let Ok(mut cache) = self.grants.lock() {
            cache.retain(|_, until| *until > Instant::now());
            cache.insert(key, Instant::now() + SAR_TTL);
        }
        allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;
    use std::sync::Arc;

    fn cluster(verb: &'static str, ns: &str) -> Access {
        Access::Cluster {
            verb,
            ns: Some(ns.to_string()),
        }
    }

    #[test]
    fn cluster_paths_map_to_verbs() {
        let t = authz_target;
        assert_eq!(
            t(&Method::GET, "/api/v1/namespaces/team-a/clusters"),
            cluster("list", "team-a")
        );
        assert_eq!(
            t(&Method::POST, "/api/v1/namespaces/team-a/clusters"),
            cluster("create", "team-a")
        );
        assert_eq!(
            t(&Method::GET, "/api/v1/namespaces/team-a/clusters/demo"),
            cluster("get", "team-a")
        );
        assert_eq!(
            t(
                &Method::GET,
                "/api/v1/namespaces/team-a/clusters/demo/backups"
            ),
            cluster("get", "team-a")
        );
        assert_eq!(
            t(
                &Method::GET,
                "/api/v1/namespaces/team-a/clusters/demo/secrets"
            ),
            cluster("get", "team-a")
        );
        assert_eq!(
            t(&Method::PATCH, "/api/v1/namespaces/team-a/clusters/demo"),
            cluster("patch", "team-a")
        );
        assert_eq!(
            t(&Method::DELETE, "/api/v1/namespaces/team-a/clusters/demo"),
            cluster("delete", "team-a")
        );
        for op in [
            "switchover",
            "restart",
            "hibernate",
            "wake",
            "rotate-credentials",
            "backups",
        ] {
            assert_eq!(
                t(
                    &Method::POST,
                    &format!("/api/v1/namespaces/team-a/clusters/demo/{op}")
                ),
                cluster("patch", "team-a"),
                "{op}"
            );
        }
        assert_eq!(
            t(
                &Method::POST,
                "/api/v1/namespaces/team-a/clusters/demo/instances/demo-1/fence"
            ),
            cluster("patch", "team-a")
        );
        assert_eq!(
            t(
                &Method::POST,
                "/api/v1/namespaces/team-a/clusters/demo/restore"
            ),
            cluster("create", "team-a")
        );
    }

    #[test]
    fn global_list_is_cluster_wide_and_other_paths_need_only_authentication() {
        assert_eq!(
            authz_target(&Method::GET, "/api/v1/clusters"),
            Access::Cluster {
                verb: "list",
                ns: None
            }
        );
        for path in [
            "/api/v1/ping",
            "/api/v1/version",
            "/api/v1/gucs",
            "/api/v1/preflight",
        ] {
            assert_eq!(
                authz_target(&Method::GET, path),
                Access::Authenticated,
                "{path}"
            );
        }
    }

    fn mapping(username_claim: &str, prefix: &str) -> SubjectMapping {
        SubjectMapping::new(&crate::config::Config {
            auth_oidc_username_claim: username_claim.into(),
            auth_oidc_username_prefix: prefix.into(),
            auth_oidc_groups_prefix: prefix.into(),
            ..crate::config::Config::test_default()
        })
    }

    #[test]
    fn claims_map_to_a_prefixed_subject() {
        let claims = serde_json::json!({
            "sub": "abc-123", "email": "alice@corp.com",
            "groups": ["platform-team", "sre"],
        });
        let s = mapping("sub", "oidc:").subject(&claims).expect("subject");
        assert_eq!(s.user, "oidc:abc-123");
        assert_eq!(s.groups, vec!["oidc:platform-team", "oidc:sre"]);

        let s = mapping("email", "oidc:").subject(&claims).expect("subject");
        assert_eq!(s.user, "oidc:alice@corp.com");
    }

    #[test]
    fn an_empty_prefix_is_applied_as_empty() {
        // Mirrors an apiserver configured without --oidc-username-prefix.
        let claims = serde_json::json!({"sub": "alice", "groups": ["devs"]});
        let s = mapping("sub", "").subject(&claims).expect("subject");
        assert_eq!(s.user, "alice");
        assert_eq!(s.groups, vec!["devs"]);
    }

    #[test]
    fn groups_accept_an_array_a_bare_string_or_nothing() {
        let m = mapping("sub", "");
        let one = serde_json::json!({"sub": "a", "groups": "solo"});
        assert_eq!(m.subject(&one).expect("subject").groups, vec!["solo"]);

        let none = serde_json::json!({"sub": "a"});
        assert!(m.subject(&none).expect("subject").groups.is_empty());

        // A malformed entry is skipped rather than poisoning the whole SAR.
        let mixed = serde_json::json!({"sub": "a", "groups": ["ok", 7, null]});
        assert_eq!(m.subject(&mixed).expect("subject").groups, vec!["ok"]);

        let wrong_type = serde_json::json!({"sub": "a", "groups": 42});
        assert!(m.subject(&wrong_type).expect("subject").groups.is_empty());
    }

    #[test]
    fn an_unusable_username_claim_yields_no_subject() {
        // Authorizing an empty user is how a mistyped claim name turns into
        // an accidental grant, so every one of these is a rejection.
        let m = mapping("sub", "oidc:");
        assert!(m.subject(&serde_json::json!({"email": "a@b.c"})).is_none());
        assert!(m.subject(&serde_json::json!({"sub": ""})).is_none());
        assert!(m.subject(&serde_json::json!({"sub": 42})).is_none());
        assert!(m.subject(&serde_json::json!({"sub": null})).is_none());
    }

    #[test]
    fn display_name_falls_back_through_the_usual_claims() {
        let m = mapping("sub", "");
        let full = serde_json::json!({
            "name": "Alice Ng", "preferred_username": "alice", "email": "a@b.c",
        });
        assert_eq!(m.display_name(&full).as_deref(), Some("Alice Ng"));
        let no_name = serde_json::json!({"preferred_username": "alice", "email": "a@b.c"});
        assert_eq!(m.display_name(&no_name).as_deref(), Some("alice"));
        let only_email = serde_json::json!({"email": "a@b.c"});
        assert_eq!(m.display_name(&only_email).as_deref(), Some("a@b.c"));
        assert_eq!(m.display_name(&serde_json::json!({"sub": "x"})), None);
    }

    /// A stand-in apiserver that records the SubjectAccessReviews it is sent
    /// and allows only members of `oidc:admins`.
    async fn spawn_apiserver() -> (kube::Client, Arc<Mutex<Vec<SubjectAccessReviewSpec>>>) {
        use axum::extract::State;
        use axum::routing::post;

        let seen: Arc<Mutex<Vec<SubjectAccessReviewSpec>>> = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let app = axum::Router::new()
            .route(
                "/apis/authorization.k8s.io/v1/subjectaccessreviews",
                post(
                    |State(seen): State<Arc<Mutex<Vec<SubjectAccessReviewSpec>>>>,
                     axum::Json(mut review): axum::Json<SubjectAccessReview>| async move {
                        let spec = std::mem::take(&mut review.spec);
                        let allowed = spec
                            .groups
                            .as_ref()
                            .is_some_and(|g| g.iter().any(|g| g == "oidc:admins"));
                        if let Ok(mut s) = seen.lock() {
                            s.push(spec);
                        }
                        review.status = Some(
                            k8s_openapi::api::authorization::v1::SubjectAccessReviewStatus {
                                allowed,
                                ..Default::default()
                            },
                        );
                        axum::Json(review)
                    },
                ),
            )
            .with_state(seen.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let cfg = kube::Config::new(
            format!("http://{addr}")
                .parse::<axum::http::Uri>()
                .expect("uri"),
        );
        (kube::Client::try_from(cfg).expect("client"), seen)
    }

    #[tokio::test]
    async fn authorize_sends_the_subject_it_is_given() {
        // Groups used to be read back out of the TokenReview cache, which the
        // OIDC path never populates — and which silently emptied the SAR if
        // the 60s entry expired mid-request. They now travel with the call.
        let (client, seen) = spawn_apiserver().await;
        let authorizer = KubernetesAuthorizer::new(client);
        let subject = Subject {
            user: "oidc:alice@corp.com".into(),
            groups: vec!["oidc:admins".into()],
        };
        let access = cluster("patch", "team-a");
        assert!(authorizer.authorize("key-1", &subject, &access).await);

        let sent = seen.lock().expect("lock");
        let spec = sent.first().expect("one review");
        assert_eq!(spec.user.as_deref(), Some("oidc:alice@corp.com"));
        assert_eq!(
            spec.groups.as_deref(),
            Some(["oidc:admins".to_string()].as_slice())
        );
        let attrs = spec.resource_attributes.as_ref().expect("attributes");
        assert_eq!(attrs.group.as_deref(), Some("pglord.dev"));
        assert_eq!(attrs.resource.as_deref(), Some("postgresclusters"));
        assert_eq!(attrs.verb.as_deref(), Some("patch"));
        assert_eq!(attrs.namespace.as_deref(), Some("team-a"));
    }

    #[tokio::test]
    async fn a_denied_subject_is_not_cached_but_a_granted_one_is() {
        let (client, seen) = spawn_apiserver().await;
        let authorizer = KubernetesAuthorizer::new(client);
        let outsider = Subject {
            user: "oidc:bob".into(),
            groups: vec!["oidc:devs".into()],
        };
        let access = cluster("delete", "team-a");
        assert!(!authorizer.authorize("key-b", &outsider, &access).await);
        assert!(!authorizer.authorize("key-b", &outsider, &access).await);
        assert_eq!(seen.lock().expect("lock").len(), 2, "denials re-checked");

        let admin = Subject {
            user: "oidc:alice".into(),
            groups: vec!["oidc:admins".into()],
        };
        assert!(authorizer.authorize("key-a", &admin, &access).await);
        assert!(authorizer.authorize("key-a", &admin, &access).await);
        assert_eq!(seen.lock().expect("lock").len(), 3, "grant cached");
    }

    #[tokio::test]
    async fn authenticated_only_access_needs_no_review() {
        let (client, seen) = spawn_apiserver().await;
        let authorizer = KubernetesAuthorizer::new(client);
        let anyone = Subject {
            user: "oidc:nobody".into(),
            groups: Vec::new(),
        };
        assert!(
            authorizer
                .authorize("key-x", &anyone, &Access::Authenticated)
                .await
        );
        assert!(seen.lock().expect("lock").is_empty());
    }

    #[test]
    fn denial_detail_names_verb_and_namespace() {
        let d = denial_detail("alice", &cluster("patch", "team-a"));
        assert!(d.contains("alice") && d.contains("patch") && d.contains("team-a"));
    }
}
