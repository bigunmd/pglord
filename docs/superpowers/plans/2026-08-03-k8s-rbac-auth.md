# Kubernetes-RBAC Auth (Operator Mode Stage C) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Kubernetes bearer tokens as pglord credentials: TokenReview authenticates, SubjectAccessReview on `pglord.dev/postgresclusters` authorizes each request, per `docs/superpowers/specs/2026-08-03-k8s-rbac-auth-design.md`.

**Architecture:** A third auth mode in the existing middleware. New `auth/kubernetes.rs`: a pure request→SAR-target mapping, a TokenReview/SAR client over the kube client, and short-TTL caches. 403 `forbidden` problem code. auth-delegator RBAC, preflight, login-page hint, e2e.

**Tech Stack:** existing kube-rs client (`authentication.k8s.io/v1` TokenReview, `authorization.k8s.io/v1` SubjectAccessReview via k8s-openapi types), sha2 for token hashing.

## Global Constraints

- Static-token and OIDC modes byte-identical; evaluation order static → OIDC → kubernetes.
- TokenReview unavailable (no kube env) = unauthenticated (401), never 500.
- Verbs check `group: pglord.dev, resource: postgresclusters` regardless of spec backend.
- Caches: TokenReview 60s (or token expiry), SAR 10s; failures never cached.
- Contract regen for the auth-config field; both install paths together.

---

### Task 1: Config + auth-config field

**Files:**
- Modify: `crates/pglord-server/src/config.rs`, `crates/pglord-server/src/api/auth.rs`

- [ ] Config: `auth_kubernetes: bool` (`#[serde(default)]`, doc: "Accept Kubernetes bearer tokens: TokenReview to authenticate, SubjectAccessReview on pglord.dev/postgresclusters to authorize. Needs the auth-delegator binding."), added to `test_default()`; `auth_enabled()` ORs it in (find it in config.rs). Jail test asserting `PGLORD_AUTH_KUBERNETES=true` parses and enables auth.
- [ ] `api/auth.rs` auth-config DTO gains `kubernetes_enabled: bool` from cfg; extend the existing http.rs auth-config test to assert `false` by default and `true` with the env set (via `test_app_with`).
- [ ] `cargo test -p pglord-server` → PASS; commit `feat(server): auth_kubernetes mode flag and auth-config exposure`.

---

### Task 2: `auth/kubernetes.rs` — mapping, reviews, caches

**Files:**
- Create: `crates/pglord-server/src/auth/kubernetes.rs`
- Modify: `crates/pglord-server/src/auth/mod.rs` (module decl)

**Interfaces (consumed by Task 3):**

```rust
pub enum Access {
    /// Authenticated is enough (ping, version, gucs, preflight).
    Authenticated,
    /// SAR on postgresclusters with this verb; ns None = cluster-wide.
    Cluster { verb: &'static str, ns: Option<String> },
}
pub fn authz_target(method: &axum::http::Method, path: &str) -> Access;

pub struct KubernetesAuthorizer { /* kube client + Mutex<HashMap> caches */ }
impl KubernetesAuthorizer {
    pub fn new(client: kube::Client) -> Self;
    /// None = token rejected. Some(user) = authenticated username.
    pub async fn authenticate(&self, token: &str) -> Option<String>;
    pub async fn authorize(&self, token_hash: &str, user: &str, groups: &[String], access: &Access) -> bool;
}
```

- [ ] **Mapping** (pure). Day-2 op suffixes = `switchover|restart|hibernate|wake|rotate-credentials`, plus `instances/{pod}/(un)fence`, plus POST `backups`; `restore` maps to `create`. Table tests covering every spec row, the global list (`GET /api/v1/clusters` → `Cluster{verb:"list", ns:None}`), GET backups/secrets → `get`, and non-cluster paths → `Authenticated`.
- [ ] **TokenReview**: `Api::<k8s_openapi::api::authentication::v1::TokenReview>::all(client).create(...)` with `spec.token`; authenticated + username extracted; cache keyed `sha2(token)` with `Instant`-based 60s TTL (respect `status.authenticated == Some(true)` only). Groups captured alongside for SAR.
- [ ] **SAR**: `authorization.k8s.io/v1 SubjectAccessReview` with `spec.user`, `spec.groups`, `resource_attributes { group: "pglord.dev", resource: "postgresclusters", verb, namespace }`; allowed cached 10s keyed (token-hash, verb, ns).
- [ ] Cache tests with injected `Instant`s (store `now: fn() -> Instant` or pass timestamps into pure cache helpers — mirror whatever is cheapest; the pure part must be tested, the I/O part stays thin).
- [ ] `cargo test -p pglord-server auth` → PASS; commit `feat(server): kubernetes token review and postgrescluster SAR mapping`.

---

### Task 3: Middleware branch + 403 + wiring

**Files:**
- Modify: `crates/pglord-server/src/auth/mod.rs`, `crates/pglord-server/src/app.rs` (pass kube client into `AuthState::from_config`), `crates/pglord-server/src/error.rs` (forbidden helper if absent)

- [ ] `AuthState` gains `pub kubernetes: Option<kubernetes::KubernetesAuthorizer>`; `from_config(cfg, http, kube: Option<kube::Client>)` builds it when `cfg.auth_kubernetes && kube.is_some()` (mode on without kube env → field None → tokens fall through to 401, honoring the never-500 constraint). Update the app.rs call site: `AuthState::from_config(cfg, reqwest::Client::new(), cluster_ops.as_ref().map(|o| o.kube.clone()))`.
- [ ] Middleware, after the OIDC branch:

```rust
    if let Some(k8s) = &auth.kubernetes {
        let access = kubernetes::authz_target(req.method(), req.uri().path());
        if let Some(user) = k8s.authenticate(token).await {
            if k8s.authorize(token, &user, &access).await {
                return next.run(req).await;
            }
            return ApiError::new(axum::http::StatusCode::FORBIDDEN, "forbidden")
                .with_detail(kubernetes::denial_detail(&user, &access))
                .into_response();
        }
    }
```

(align exact signatures with Task 2's; `denial_detail` produces "user 'alice' cannot patch postgresclusters in team-a").
- [ ] http.rs integration test: kubernetes mode on, no kube env → bearer answers 401 `auth_invalid`; static token still passes.
- [ ] `cargo test -p pglord-server && make lint` → PASS; commit `feat(server): kubernetes auth branch — TokenReview + SAR with 403 forbidden`.

---

### Task 4: RBAC, chart, preflight

**Files:**
- Modify: `deploy/k8s/operator/rbac.yaml` (+ auth-delegator ClusterRoleBinding block), `deploy/k8s/deployment.yaml` (commented env), `charts/pglord/values.yaml` (`auth.kubernetes: false`), `charts/pglord/templates/deployment.yaml` (env), `charts/pglord/templates/rbac.yaml` (delegator binding when `auth.kubernetes`), `crates/pglord-server/src/preflight.rs` (+ callers)

- [ ] auth-delegator binding (both raw operator file and chart, chart-gated on `.Values.auth.kubernetes`):

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: pglord-auth-delegator
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: system:auth-delegator
subjects:
  - kind: ServiceAccount
    name: pglord-server
    namespace: pglord
```

- [ ] `preflight::required(spec_backend, auth_kubernetes)` appends `authentication.k8s.io/tokenreviews create` and `authorization.k8s.io/subjectaccessreviews create` when the mode is on; callers pass `cfg/ops` values; unit test for presence/absence.
- [ ] `helm template` sanity both ways; `cargo test && make lint` → PASS; commit `feat(deploy): auth-delegator binding, kubernetes auth env and preflight checks`.

---

### Task 5: Contract + UI hint

**Files:**
- Regenerate: `spec/openapi.yaml`, `ui/src/app/api/**`
- Modify: `ui/src/app/pages/login/login.component.ts`, `ui/public/i18n/en.json`

- [ ] `make openapi ui-client`; login component: when `auth_config.kubernetes_enabled`, show a muted hint under the token field: i18n `login.kubernetesHint` = "Kubernetes tokens work here: kubectl create token <serviceaccount>". No new inputs — the existing token field carries it.
- [ ] `make ui-lint ui-build ui-test` → clean; commit `feat(ui): kubernetes token hint on login` + `chore(contract): regenerate openapi + ts client for kubernetes auth`.

---

### Task 6: E2E + user guide

**Files:**
- Create: `scripts/rbac-auth-e2e.sh`
- Modify: `Makefile`, `docs/user-guide.md`

- [ ] Script (operator-e2e harness conventions, port 18099; pitfalls memory applies). Flow: operator mode + `PGLORD_AUTH_KUBERNETES=true` + a static admin token (`PGLORD_AUTH_TOKENS=e2e-admin`) → apply auth-delegator binding → create ns team-a/team-b + SA `alice` in team-a + Role get/list/patch on postgresclusters + binding → CR `demo` in each ns (via admin token) → `TOKEN=$(kubectl -n team-a create token alice)` → assertions:
  - `GET /api/v1/namespaces/team-a/clusters` with alice → 200;
  - `PATCH .../team-a/clusters/demo` (parameters) → 200;
  - `GET .../team-b/clusters/demo` → 403 + code `forbidden`;
  - `GET /api/v1/clusters` → 403 (no cluster-wide list);
  - `DELETE .../team-a/clusters/demo` → 403 (no delete verb);
  - no token → 401; garbage token → 401; admin static token → 200 everywhere.
  - Cleanup: namespaces, bindings, env reset.
- [ ] Makefile target + help; run `make rbac-auth-e2e` on the stand.
- [ ] User guide: "Kubernetes-native authentication" — enable flag, delegator binding, SA/Role/token recipe, verb table, global-list caveat, 401 vs 403 semantics.
- [ ] Commit `test(e2e): kubernetes rbac auth flow` and `docs(user-guide): kubernetes-native authentication`.

---

## Self-review notes

- Spec coverage: mode/config → T1; mapping/reviews/caches → T2; middleware/403/never-500 → T3; delegator/preflight/chart → T4; auth-config+login → T5 (+T1 server side); e2e/docs → T6.
- Type consistency: `authz_target`, `Access`, `KubernetesAuthorizer` defined T2, consumed T3; `required(spec_backend, auth_kubernetes)` contained in T4.
- The `authorize(token, user, access)` middleware call vs Task 2's `(token_hash, user, groups, access)` signature: reconcile during T3 — the authorizer should own hashing and group storage internally; final signature `authorize(&self, token: &str, user: &str, access: &Access) -> bool` with groups looked up from the auth cache entry.
