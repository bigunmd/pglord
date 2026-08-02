# PgBouncer Pooling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Optional managed PgBouncer in front of each cluster's primary: Deployment `<name>-pooler` + Service `<name>-pool:5432`, `auth_query` client auth via a managed `pglord_pooler` role, TLS-following.

**Architecture:** New `pooler` feature block on `ClusterSpec` (backup-style `enabled` flag). `pglord-core` renders pgbouncer.ini/userlist + Deployment/Service/Secret purely; the reconciler applies them (deletes on disable — a first) and level-triggered-ensures the auth role via a new agent endpoint `POST /pooler/ensure`. Spec: `docs/superpowers/specs/2026-08-02-pgbouncer-design.md`.

**Tech Stack:** Rust (axum, kube-rs, k8s-openapi, tokio-postgres), Angular + spartan hlm + Transloco, pgbouncer/pgbouncer:1.24.1 image.

## Global Constraints

- Rust edition 2024; clippy denies warnings and `unwrap_used` — use `expect` only in tests.
- Never hand-edit `spec/openapi.yaml` or `ui/src/app/api/**`; run `make openapi ui-client` after API changes and commit regenerated artifacts with the code.
- All user-visible UI strings go through Transloco (`ui/public/i18n/en.json`).
- `deploy/k8s/` and `charts/pglord/` must receive identical RBAC changes.
- Verify each task: `cargo test -p <crate>` then `make lint` before commit.
- Commit style: `feat(core): …`, `feat(agent): …`, `feat(server): …`, `feat(ui): …`, `chore(contract): …`, `test(e2e): …`, `docs(user-guide): …`.

---

### Task 1: `pooler` spec block + validation + apply_change

**Files:**
- Create: `crates/pglord-core/src/pooler.rs`
- Modify: `crates/pglord-core/src/lib.rs` (add `pub mod pooler;`)
- Modify: `crates/pglord-core/src/spec.rs` (field on `ClusterSpec`, `SpecChange`, `validate`, `apply_change`)

**Interfaces:**
- Produces: `pooler::PoolerSpec { enabled: bool, replicas: u8, pool_mode: PoolMode, max_client_conn: u32, default_pool_size: u32, image: String, resources: workload::Resources }` (serde defaults: false, 1, Transaction, 100, 20, `"pgbouncer/pgbouncer:1.24.1"`, empty). `PoolMode::{Transaction, Session}` (`rename_all = "lowercase"`). `pooler::validate(&PoolerSpec) -> Result<(), PoolerError>`; `PoolerError::{InvalidReplicas(u8), InvalidMaxClientConn(u32), InvalidPoolSize(u32), InvalidImage}` (thiserror). Constants `pooler::POOLER_ROLE: &str = "pglord_pooler"`.
- `ClusterSpec` gains `#[serde(default)] pub pooler: crate::pooler::PoolerSpec`; `SpecChange` gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub pooler: Option<crate::pooler::PoolerSpec>` with replace-wholesale semantics (mirror the existing `tls` arm); `ClusterSpec::validate` calls `pooler::validate`. `SpecError` gains `#[error(transparent)] Pooler(#[from] PoolerError)` (mirror how `WorkloadError` is wrapped).

- [ ] **Step 1: Write failing tests** in `pooler.rs` `#[cfg(test)]` and `spec.rs` tests:

```rust
// pooler.rs tests
#[test]
fn defaults_are_disabled_transaction_mode() {
    let p: PoolerSpec = serde_json::from_value(serde_json::json!({})).expect("pooler");
    assert!(!p.enabled);
    assert_eq!(p.replicas, 1);
    assert_eq!(p.pool_mode, PoolMode::Transaction);
    assert_eq!(p.max_client_conn, 100);
    assert_eq!(p.default_pool_size, 20);
    assert_eq!(p.image, "pgbouncer/pgbouncer:1.24.1");
}

#[test]
fn bounds_are_enforced() {
    let ok: PoolerSpec = serde_json::from_value(serde_json::json!({ "enabled": true })).expect("p");
    assert!(validate(&ok).is_ok());
    let mut p = ok.clone();
    p.replicas = 0;
    assert!(matches!(validate(&p), Err(PoolerError::InvalidReplicas(0))));
    let mut p = ok.clone();
    p.replicas = 11;
    assert!(matches!(validate(&p), Err(PoolerError::InvalidReplicas(11))));
    let mut p = ok.clone();
    p.max_client_conn = 0;
    assert!(matches!(validate(&p), Err(PoolerError::InvalidMaxClientConn(0))));
    let mut p = ok.clone();
    p.default_pool_size = 0;
    assert!(matches!(validate(&p), Err(PoolerError::InvalidPoolSize(0))));
    let mut p = ok;
    p.image = "  ".into();
    assert!(matches!(validate(&p), Err(PoolerError::InvalidImage)));
}

#[test]
fn disabled_pooler_with_bad_knobs_still_validates() {
    // Same stance as backup: inert knobs must not block an unrelated spec save.
    let mut p: PoolerSpec = serde_json::from_value(serde_json::json!({})).expect("p");
    p.replicas = 0;
    assert!(validate(&p).is_ok());
}
```

```rust
// spec.rs test
#[test]
fn pooler_block_is_replace_wholesale() {
    let base = spec("demo");
    let change: SpecChange = serde_json::from_value(serde_json::json!({
        "pooler": { "enabled": true, "replicas": 2 }
    }))
    .expect("change");
    let with = base.apply_change(&change).expect("apply");
    assert!(with.pooler.enabled);
    assert_eq!(with.pooler.replicas, 2);
    // Absent leaves untouched; explicit default block disables.
    assert!(with.apply_change(&SpecChange::default()).expect("a").pooler.enabled);
    let clear: SpecChange =
        serde_json::from_value(serde_json::json!({ "pooler": {} })).expect("change");
    assert!(!with.apply_change(&clear).expect("a").pooler.enabled);
    // apply_change validates: out-of-bounds knobs are rejected.
    let bad: SpecChange = serde_json::from_value(serde_json::json!({
        "pooler": { "enabled": true, "replicas": 0 }
    }))
    .expect("change");
    assert!(base.apply_change(&bad).is_err());
}
```

- [ ] **Step 2:** `cargo test -p pglord-core pooler` → FAIL (module missing)
- [ ] **Step 3:** Implement `pooler.rs` (struct, enum, defaults via `fn default_*`, validate with bounds `replicas 1..=10`, `max_client_conn 1..=100_000`, `default_pool_size 1..=10_000`, image non-blank when enabled — validate returns Ok early when `!enabled`); wire `spec.rs` (field, SpecChange arm `if let Some(pooler) = &change.pooler { next.pooler = pooler.clone(); }`, validate call). Derives: `Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema` — every spec block derives `ToSchema` for the contract.
- [ ] **Step 4:** `cargo test -p pglord-core` → PASS; `make lint`
- [ ] **Step 5:** Commit: `feat(core): pooler spec block with curated knobs and strict validation`

---

### Task 2: pgbouncer.ini + userlist render (pure, in pooler.rs)

**Files:**
- Modify: `crates/pglord-core/src/pooler.rs`

**Interfaces:**
- Consumes: `ClusterSpec` (for name, pooler knobs, `tls.enabled()`), `render::PG_PORT`.
- Produces: `pooler::pgbouncer_ini(spec: &ClusterSpec) -> String`, `pooler::userlist(password: &str) -> String`, `pooler::PGBOUNCER_PORT: i32 = 6432`, `pooler::config_hash(ini: &str, userlist: &str) -> String` (sha256 hex). Add `sha2 = "0.10"` to `crates/pglord-core/Cargo.toml` (workspace-style like other deps).

- [ ] **Step 1: Write failing tests:**

```rust
#[test]
fn ini_targets_rw_service_with_auth_query() {
    let spec: crate::spec::ClusterSpec = serde_json::from_value(
        serde_json::json!({ "name": "demo", "pooler": { "enabled": true } }),
    )
    .expect("spec");
    let ini = pgbouncer_ini(&spec);
    assert!(ini.contains("* = host=demo-rw port=5432 auth_user=pglord_pooler"));
    assert!(ini.contains("listen_port = 6432"));
    assert!(ini.contains("auth_type = scram-sha-256"));
    assert!(ini.contains("auth_query = SELECT usename, passwd FROM pglord.pooler_lookup($1)"));
    assert!(ini.contains("auth_dbname = postgres"));
    assert!(ini.contains("pool_mode = transaction"));
    assert!(ini.contains("max_client_conn = 100"));
    assert!(ini.contains("default_pool_size = 20"));
    assert!(!ini.contains("client_tls"), "no tls lines on a plain cluster");
}

#[test]
fn tls_cluster_gets_client_and_server_tls_lines() {
    let spec: crate::spec::ClusterSpec = serde_json::from_value(serde_json::json!({
        "name": "demo",
        "pooler": { "enabled": true },
        "tls": { "server_secret": "demo-tls" }
    }))
    .expect("spec");
    let ini = pgbouncer_ini(&spec);
    assert!(ini.contains("client_tls_sslmode = allow"));
    assert!(ini.contains("client_tls_cert_file = /etc/pglord/tls/tls.crt"));
    assert!(ini.contains("client_tls_key_file = /etc/pglord/tls/tls.key"));
    assert!(ini.contains("server_tls_sslmode = require"));
}

#[test]
fn userlist_holds_exactly_the_pooler_role() {
    assert_eq!(userlist("pw-123"), "\"pglord_pooler\" \"pw-123\"\n");
}

#[test]
fn config_hash_changes_with_content() {
    let a = config_hash("ini-a", "ul");
    assert_eq!(a, config_hash("ini-a", "ul"), "deterministic");
    assert_ne!(a, config_hash("ini-b", "ul"));
    assert_ne!(a, config_hash("ini-a", "ul2"));
    assert_eq!(a.len(), 64);
}
```

- [ ] **Step 2:** `cargo test -p pglord-core pooler` → FAIL
- [ ] **Step 3:** Implement. ini template (note `session` renders from `PoolMode` via a `as_ini(&self) -> &'static str`):

```rust
pub fn pgbouncer_ini(spec: &ClusterSpec) -> String {
    let p = &spec.pooler;
    let mut ini = format!(
        "[databases]\n\
         * = host={name}-rw port={pg} auth_user={role}\n\n\
         [pgbouncer]\n\
         listen_addr = *\n\
         listen_port = {port}\n\
         auth_type = scram-sha-256\n\
         auth_file = /etc/pgbouncer/userlist.txt\n\
         auth_user = {role}\n\
         auth_query = SELECT usename, passwd FROM pglord.pooler_lookup($1)\n\
         auth_dbname = postgres\n\
         pool_mode = {mode}\n\
         max_client_conn = {mcc}\n\
         default_pool_size = {dps}\n\
         ignore_startup_parameters = extra_float_digits,options\n",
        name = spec.name, pg = crate::render::PG_PORT, role = POOLER_ROLE,
        port = PGBOUNCER_PORT, mode = p.pool_mode.as_ini(),
        mcc = p.max_client_conn, dps = p.default_pool_size,
    );
    if spec.tls.enabled() {
        ini.push_str(
            "client_tls_sslmode = allow\n\
             client_tls_cert_file = /etc/pglord/tls/tls.crt\n\
             client_tls_key_file = /etc/pglord/tls/tls.key\n\
             server_tls_sslmode = require\n",
        );
    }
    ini
}
```

`config_hash`: `sha2::Sha256` over `ini`, a `\0` separator, `userlist`; hex via `format!("{:x}", …)`. Check `render::PG_PORT` visibility — make `pub` if it isn't.
- [ ] **Step 4:** `cargo test -p pglord-core` → PASS; `make lint`
- [ ] **Step 5:** Commit: `feat(core): pgbouncer ini and userlist render, tls-following`

---

### Task 3: Deployment / Service / config-Secret render

**Files:**
- Modify: `crates/pglord-core/src/render.rs`

**Interfaces:**
- Consumes: `pooler::{pgbouncer_ini, userlist, config_hash, PGBOUNCER_PORT}`, existing `meta()`, `labels()`, `OwnerRef`.
- Produces (all in `render.rs`):
  - `pub const POOLER_PASSWORD_KEY: &str = "pooler-password";`
  - `pub fn pooler_deployment_name(cluster: &str) -> String` → `{cluster}-pooler`
  - `pub fn pool_service_name(cluster: &str) -> String` → `{cluster}-pool`
  - `pub fn pooler_config_secret_name(cluster: &str) -> String` → `{cluster}-pooler-config`
  - `pub fn pooler_labels(cluster: &str) -> BTreeMap<String, String>` — `labels()` plus `("pglord.dev/role", "pooler")`; instance selectors are unaffected because the StatefulSet selector doesn't select on this key, but the pooler Deployment selector uses the full pooler set so it can never adopt instance pods.
  - `pub fn render_pooler_config_secret(spec, ns, owner, pooler_password: &str) -> Secret` — keys `pgbouncer.ini`, `userlist.txt` via `string_data`.
  - `pub fn render_pooler_deployment(spec, ns, owner, config_hash: &str) -> Deployment` — replicas from spec, pooler labels + selector, pod-template annotation `pglord.dev/config-hash: <hash>`, container `pgbouncer` running `spec.pooler.image`, port 6432, config Secret volume mounted read-only at `/etc/pgbouncer`, TCP readiness probe on 6432, resources mapped exactly like the instance container maps `spec.workload.resources`; when `spec.tls.enabled()` also mount the TLS Secret read-only at `/etc/pglord/tls` (non-optional, `default_mode: 0o640` — copy the instance mount block).
  - `pub fn render_pool_service(spec, ns, owner) -> Service` — port 5432 → targetPort 6432, selector `pooler_labels`.

- [ ] **Step 1: Write failing tests** (in `render.rs` tests, reuse `demo()` helper but give it a pooler-enabled variant):

```rust
#[test]
fn pooler_objects_are_rendered_with_owner_and_hash() {
    let spec: ClusterSpec = serde_json::from_value(serde_json::json!({
        "name": "demo", "pooler": { "enabled": true, "replicas": 2 }
    }))
    .expect("spec");
    let owner = demo().1;
    let secret = render_pooler_config_secret(&spec, "ns", &owner, "pw");
    assert_eq!(secret.metadata.name.as_deref(), Some("demo-pooler-config"));
    let data = secret.string_data.expect("string_data");
    assert!(data["pgbouncer.ini"].contains("host=demo-rw"));
    assert_eq!(data["userlist.txt"], "\"pglord_pooler\" \"pw\"\n");

    let dep = render_pooler_deployment(&spec, "ns", &owner, "abc123");
    assert_eq!(dep.metadata.name.as_deref(), Some("demo-pooler"));
    let dspec = dep.spec.expect("spec");
    assert_eq!(dspec.replicas, Some(2));
    let tmpl = dspec.template;
    assert_eq!(
        tmpl.metadata.expect("meta").annotations.expect("ann")["pglord.dev/config-hash"],
        "abc123"
    );
    let pod = tmpl.spec.expect("pod");
    assert_eq!(pod.containers[0].image.as_deref(), Some("pgbouncer/pgbouncer:1.24.1"));
    assert!(pod.volumes.expect("vols").iter().any(|v| v.secret.is_some()));

    let svc = render_pool_service(&spec, "ns", &owner);
    assert_eq!(svc.metadata.name.as_deref(), Some("demo-pool"));
    let sspec = svc.spec.expect("spec");
    assert_eq!(sspec.selector.expect("sel")["pglord.dev/role"], "pooler");
    let port = &sspec.ports.expect("ports")[0];
    assert_eq!(port.port, 5432);
    assert_eq!(port.target_port, Some(IntOrString::Int(6432)));
}

#[test]
fn tls_pooler_mounts_the_cert_secret() {
    let spec: ClusterSpec = serde_json::from_value(serde_json::json!({
        "name": "demo", "pooler": { "enabled": true },
        "tls": { "server_secret": "demo-tls" }
    }))
    .expect("spec");
    let dep = render_pooler_deployment(&spec, "ns", &demo().1, "h");
    let pod = dep.spec.expect("s").template.spec.expect("pod");
    let vols = pod.volumes.expect("vols");
    assert!(vols.iter().any(|v| {
        v.secret.as_ref().and_then(|s| s.secret_name.as_deref()) == Some("demo-tls")
    }));
    let mounts = pod.containers[0].volume_mounts.clone().expect("mounts");
    assert!(mounts.iter().any(|m| m.mount_path == "/etc/pglord/tls" && m.read_only == Some(true)));
}

#[test]
fn every_pooler_object_carries_owner_refs() {
    // Extend the existing every_object_carries_owner_and_labels test's list with
    // the three pooler objects' metadata.
}
```

- [ ] **Step 2:** `cargo test -p pglord-core render` → FAIL
- [ ] **Step 3:** Implement, importing `k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec}`. Follow the existing statefulset container/volume construction style verbatim.
- [ ] **Step 4:** `cargo test -p pglord-core` → PASS; `make lint`
- [ ] **Step 5:** Commit: `feat(core): render pooler deployment, pool service and config secret`

---

### Task 4: deps.rs — `pooler-password` required key

**Files:**
- Modify: `crates/pglord-core/src/deps.rs`

**Interfaces:**
- Consumes: `render::POOLER_PASSWORD_KEY`.
- Produces: when `spec.pooler.enabled`, the Credentials entry's `keys` additionally contains `pooler-password` (managed or user-supplied alike — the required-secrets endpoint then flags a user Secret missing it with zero new server code).

- [ ] **Step 1: Write failing test:**

```rust
#[test]
fn an_enabled_pooler_requires_the_pooler_password_key() {
    let r = required_resources(&spec(serde_json::json!({
        "name": "demo",
        "credentials_secret": "my-creds",
        "pooler": { "enabled": true }
    })));
    assert!(r[0].keys.contains(&"pooler-password".to_string()));

    let r = required_resources(&spec(serde_json::json!({ "name": "demo" })));
    assert!(!r[0].keys.contains(&"pooler-password".to_string()));
}
```

- [ ] **Step 2:** `cargo test -p pglord-core deps` → FAIL
- [ ] **Step 3:** In the credentials `push`, build the key vec conditionally:

```rust
let mut keys = vec![
    SUPERUSER_PASSWORD_KEY.to_string(),
    REPLICATION_PASSWORD_KEY.to_string(),
];
if spec.pooler.enabled {
    keys.push(POOLER_PASSWORD_KEY.to_string());
}
```

- [ ] **Step 4:** `cargo test -p pglord-core` → PASS; `make lint`
- [ ] **Step 5:** Commit: `feat(core): pooler password key required on the credentials secret`

---

### Task 5: agent `POST /pooler/ensure`

**Files:**
- Modify: `crates/pglord-agent/src/postboot.rs` (add `ensure_pooler`)
- Modify: `crates/pglord-agent/src/http.rs` (route + handler + tests)

**Interfaces:**
- Consumes: `postboot::{connect_local, is_safe_password}` (make `connect_local` `pub(crate)` if needed).
- Produces: `POST /pooler/ensure` body `{ "password": "<string>" }` → 200 `{"status":"applied"}` / 400 bad charset / 409 not provably primary / 500 with error. `postboot::ensure_pooler(cfg: &Config, password: &str) -> anyhow::Result<()>`.

- [ ] **Step 1: Write failing handler tests** (mirror the two `/roles/apply` tests):

```rust
#[tokio::test]
async fn pooler_ensure_rejects_unsafe_passwords_without_touching_postgres() {
    let resp = test_router()
        .oneshot(
            Request::post("/pooler/ensure")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"password":"has$pglord$end"}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pooler_ensure_refuses_when_not_provably_primary() {
    let resp = test_router()
        .oneshot(
            Request::post("/pooler/ensure")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"password":"aB3-x_9"}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}
```

- [ ] **Step 2:** `cargo test -p pglord-agent http` → FAIL (404 route)
- [ ] **Step 3:** Implement. Handler shape copies `apply_roles` (charset check → `in_recovery != Some(false)` → work). `ensure_pooler` runs, over one local connection against `dbname=postgres`:

```rust
pub async fn ensure_pooler(cfg: &Config, password: &str) -> anyhow::Result<()> {
    if !is_safe_password(password) {
        anyhow::bail!("pooler password may only contain [A-Za-z0-9_-]");
    }
    let (client, conn_task) = connect_local(cfg).await?;
    let exists: bool = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = 'pglord_pooler')",
            &[],
        )
        .await?
        .get(0);
    let verb = if exists { "ALTER" } else { "CREATE" };
    client
        .batch_execute(&format!(
            "{verb} ROLE \"pglord_pooler\" WITH LOGIN PASSWORD $pglord${password}$pglord$"
        ))
        .await?;
    client
        .batch_execute(
            "CREATE SCHEMA IF NOT EXISTS pglord;\n\
             REVOKE ALL ON SCHEMA pglord FROM PUBLIC;\n\
             GRANT USAGE ON SCHEMA pglord TO \"pglord_pooler\";\n\
             CREATE OR REPLACE FUNCTION pglord.pooler_lookup(uname text)\n\
             RETURNS TABLE (usename name, passwd text)\n\
             LANGUAGE sql SECURITY DEFINER AS\n\
             $fn$ SELECT usename, passwd FROM pg_shadow WHERE usename = uname $fn$;\n\
             REVOKE ALL ON FUNCTION pglord.pooler_lookup(text) FROM PUBLIC;\n\
             GRANT EXECUTE ON FUNCTION pglord.pooler_lookup(text) TO \"pglord_pooler\";",
        )
        .await?;
    drop(client);
    conn_task.abort();
    Ok(())
}
```

(`connect_local` targets `dbname=postgres` already — matching `auth_dbname = postgres` in the ini.)
- [ ] **Step 4:** `cargo test -p pglord-agent` → PASS; `make lint`
- [ ] **Step 5:** Commit: `feat(agent): POST /pooler/ensure — auth role, schema and scram lookup function`

---

### Task 6: reconciler — ensure key, apply/delete pooler objects, ensure auth

**Files:**
- Modify: `crates/pglord-server/src/reconciler.rs`

**Interfaces:**
- Consumes: Task 3 render fns + names, `render::POOLER_PASSWORD_KEY`, `pooler::{pgbouncer_ini, userlist, config_hash}`, `render::{AGENT_HTTP_PORT, pod_fqdn}`, `primary_record::recorded_primary`.
- Produces:
  - `ensure_credentials` also: on a **managed** Secret that exists but lacks `pooler-password` while `spec.pooler.enabled`, merge-patch the key in (create-only stance kept for the other keys); on create, include `pooler-password` when the pooler is enabled.
  - `pub async fn apply_pooler(client: &kube::Client, http: &reqwest::Client, ns: &str, spec: &ClusterSpec, owner: &OwnerRef, primary_pod: &str) -> anyhow::Result<()>` — when enabled: read `pooler-password` from the credentials Secret (`spec.credentials_secret()` name or managed name; bail with a clear error when the key is missing — the pass logs it, postgres objects are already applied), render + SSA-apply Secret/Deployment/Service, then best-effort `POST http://<primary_fqdn>:<AGENT_HTTP_PORT>/pooler/ensure` (non-2xx/connect error → `tracing::warn!`, not an error — level-triggered retry next pass). When disabled: delete the three objects, 404 = success.
  - `apply_cluster` gains an `http: &reqwest::Client` parameter and calls `apply_pooler` last; `run()` constructs one `reqwest::Client` and `sync_all` threads it through. Update the one other `apply_cluster` caller (grep for it) accordingly.

- [ ] **Step 1: Write failing pure tests** (kube I/O stays untested at unit level, matching the file's existing style — test the pure decision helpers you extract):

```rust
#[test]
fn pooler_object_names_follow_the_cluster() {
    assert_eq!(
        pglord_core::render::pooler_deployment_name("demo"),
        "demo-pooler"
    );
    assert_eq!(pglord_core::render::pool_service_name("demo"), "demo-pool");
    assert_eq!(
        pglord_core::render::pooler_config_secret_name("demo"),
        "demo-pooler-config"
    );
}
```

(The real coverage for apply/delete lands in the e2e task; this task's safety net is `cargo build` + clippy + the existing reconciler tests still passing.)
- [ ] **Step 2:** `cargo test -p pglord-server reconciler` → FAIL (names not exported yet if Task 3 skipped — otherwise passes; then proceed on build errors as the signal)
- [ ] **Step 3:** Implement. Deletion arm:

```rust
async fn delete_pooler_objects(client: &kube::Client, ns: &str, cluster: &str) {
    use k8s_openapi::api::apps::v1::Deployment;
    let deps: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let svcs: Api<Service> = Api::namespaced(client.clone(), ns);
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    for result in [
        deps.delete(&pooler_deployment_name(cluster), &Default::default())
            .await
            .map(|_| ()),
        svcs.delete(&pool_service_name(cluster), &Default::default())
            .await
            .map(|_| ()),
        secrets
            .delete(&pooler_config_secret_name(cluster), &Default::default())
            .await
            .map(|_| ()),
    ] {
        match result {
            Ok(()) => {}
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(err) => tracing::warn!(cluster, %err, "pooler object delete failed"),
        }
    }
}
```

- [ ] **Step 4:** `cargo test -p pglord-server` + `cargo test --workspace` → PASS; `make lint`
- [ ] **Step 5:** Commit: `feat(server): reconcile pooler deployment/service/secret; ensure auth via agent`

---

### Task 7: rotation rotates `pooler-password`

**Files:**
- Modify: `crates/pglord-server/src/api/secrets.rs`

**Interfaces:**
- Consumes: agent `/pooler/ensure` (Task 5), `render::POOLER_PASSWORD_KEY`.
- Produces: `run_rotation` generates a third password when `spec.pooler.enabled`, includes it in the Secret merge-patch, and after `/roles/apply` succeeds POSTs `/pooler/ensure` with it. The pooler config Secret re-render (hash change → pod roll) happens on the next reconcile pass automatically — no extra code, but note it in a comment.

- [ ] **Step 1:** No new unit test is practical here (the fn is pure I/O against kube + agent, matching existing untested style); the e2e task asserts rotation end-to-end. Write the code:

```rust
let pooler_pw = spec.pooler.enabled.then(crate::reconciler::generate_password);
// in the stringData map:
if let Some(pw) = &pooler_pw {
    // (build the map as a mutable serde_json::Map to insert conditionally)
}
// after the /roles/apply block:
if let Some(pw) = &pooler_pw {
    let url = format!(
        "http://{}:{}/pooler/ensure",
        pod_fqdn(primary, &spec.name, &ops.ns),
        AGENT_HTTP_PORT
    );
    let resp = ops.http.post(&url).json(&serde_json::json!({ "password": pw })).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("agent /pooler/ensure answered {}", resp.status());
    }
}
```

- [ ] **Step 2:** `cargo test --workspace` → PASS; `make lint`
- [ ] **Step 3:** Commit: `feat(server): credentials rotation also rotates the pooler auth role`

---

### Task 8: cluster detail + SpecChange contract, regen

**Files:**
- Modify: `crates/pglord-server/src/api/clusters.rs`
- Regenerate: `spec/openapi.yaml`, `ui/src/app/api/**` (via make — never by hand)

**Interfaces:**
- Consumes: `PoolerSpec` (already `ToSchema`), Deployment status via `Api<Deployment>`.
- Produces: cluster-detail DTO gains

```rust
#[derive(Serialize, ToSchema)]
pub struct PoolerStatus {
    /// The pooler block as configured.
    pub spec: pglord_core::pooler::PoolerSpec,
    /// Ready pooler pods, from the Deployment status (0 when absent).
    pub ready_replicas: i32,
    /// Service clients connect to, `<name>-pool`, port 5432.
    pub service: String,
}
```

  populated in the detail handler: `spec: spec.pooler.clone()`, `ready_replicas` from `deployments.get_opt(&pooler_deployment_name(&name))` → `.status.ready_replicas.unwrap_or(0)`, `service: pool_service_name(&name)`. Field `pooler: PoolerStatus` on the detail response (always present — a disabled pooler reports `spec.enabled == false`, ready 0). `SpecChange` passthrough already works from Task 1; the spec-change handler needs no new code beyond the validation error mapping: map `SpecError::Pooler(_)` to 422 code `invalid_pooler` (follow the existing SpecError→ApiError match arm pattern in that file).

- [ ] **Step 1:** Find the detail DTO struct and the SpecError→ApiError mapping in `api/clusters.rs` (grep `SpecError::`), add the pieces, extend the utoipa `#[utoipa::path]` response schema automatically via the DTO change.
- [ ] **Step 2:** `cargo test --workspace` → PASS; `make lint`
- [ ] **Step 3:** `make openapi ui-client && make openapi-check ui-client-check` → clean
- [ ] **Step 4:** Commit code: `feat(server): pooler block on cluster detail, spec-change validation codes`
- [ ] **Step 5:** Commit artifacts: `chore(contract): regenerate openapi + ts client for pooler`

---

### Task 9: UI — pooler card + dialog + create-form section

**Files:**
- Create: `ui/src/app/pages/clusters/cluster-pooler-dialog.component.ts`
- Modify: `ui/src/app/pages/clusters/cluster-detail.component.ts` (pooler card)
- Modify: `ui/src/app/pages/clusters/cluster-create-dialog.component.ts` (collapsed pooler section)
- Modify: `ui/public/i18n/en.json`

**Interfaces:**
- Consumes: generated client models (`PoolerStatus`, `PoolerSpec` fields from Task 8's regen) and the existing spec-change call the other dialogs use.
- Produces: a card on cluster detail showing enabled state, `readyReplicas`/`replicas`, pool mode, and the connection hint `<name>-pool:5432`; an edit dialog (pattern-copy `cluster-tls-dialog.component.ts` — form fields: enabled toggle, replicas, pool mode select, max client conn, default pool size; sends `{ pooler: { … } }` as SpecChange); create dialog gains the same fields collapsed, default off.

- [ ] **Step 1:** Copy the TLS dialog component as the skeleton; wire inputs to a typed form; strings under a `pooler` key in `en.json` (e.g. `pooler.title`, `pooler.enable`, `pooler.poolMode.transaction`, `pooler.poolMode.session`, `pooler.replicas`, `pooler.maxClientConn`, `pooler.defaultPoolSize`, `pooler.connectVia`).
- [ ] **Step 2:** `make ui-lint ui-build` → clean. `make ui-test` if the touched components have specs.
- [ ] **Step 3:** Commit: `feat(ui): pooler card and dialog with connection hint`

---

### Task 10: RBAC — deployments verbs on both install paths

**Files:**
- Modify: `deploy/k8s/rbac.yaml`
- Modify: `charts/pglord/templates/rbac.yaml`
- Modify: `crates/pglord-server/src/preflight.rs` (add `apps/deployments` to the checked verbs so preflight honestly reports the new requirement — grep how `statefulsets` is listed and mirror it)

- [ ] **Step 1:** In both rbac files, extend the `apiGroups: [apps]` rule: `resources: [statefulsets, deployments]` (verbs already include get/list/watch/create/patch; add `delete` to this rule if not present — pooler disable deletes objects; also confirm the core-group rule's verbs cover `delete` for services/secrets, add if missing).
- [ ] **Step 2:** Update preflight checks + its tests; `cargo test -p pglord-server preflight` → PASS; `make lint`
- [ ] **Step 3:** Commit: `feat(deploy): deployments rbac for the pooler in chart and raw manifests`

---

### Task 11: e2e + user guide

**Files:**
- Create: `scripts/pooler-e2e.sh` (pattern-copy `scripts/config-e2e.sh` for harness/asserts style)
- Modify: `Makefile` (target `pooler-e2e`, listed in help)
- Modify: `docs/user-guide.md` (Pooling section)

**Flow the script must assert (against the minikube stand):**
1. Create cluster `poolt` with `pooler.enabled=true`, an init script creating role `app_rw` with a password and a database.
2. `demo-pool` Service exists; `psql "host=poolt-pool user=postgres sslmode=disable"` works (via `kubectl run` client pod as the other suites do).
3. Transaction pooling observable: open 10 concurrent client connections through the pool running `SELECT pg_sleep(2)` staggered, then `SELECT count(*) FROM pg_stat_activity WHERE usename='postgres' AND application_name LIKE 'pgbouncer%'` on the primary is < 10 (compare against `default_pool_size`).
4. `app_rw` (init-script role) connects through the pool — proves `auth_query` path.
5. Knob change (`default_pool_size` 20→5) rolls only pooler pods: instance pod `restartCount`/UIDs unchanged, pooler pod template hash changed.
6. Failover: delete the primary pod while a pooled connection loops `SELECT 1`; loop recovers within the same bound the failover-e2e suite uses.
7. Rotate credentials; `psql` through the pool still works afterwards (pooler password rotated + config rolled).
8. Disable pooler via spec change: the three objects go away; cluster still healthy.
   (TLS interplay is asserted by extending `scripts/tls-e2e.sh` with one pooled `sslmode=require` psql — small diff, same commit.)

**User guide section covers:** enabling, `<name>-pool:5432`, transaction vs session mode and the prepared-statement caveat, auth_query (app roles just work; no per-role config), TLS note — cert SANs must include `<name>-pool`, cert rotation needs `kubectl rollout restart deployment/<name>-pooler`, pooler disable leaves the `pglord_pooler` role in place.

- [ ] **Step 1:** Write the script + Makefile target; run `make pooler-e2e` on the stand until green. Memory notes for the stand apply: after a roll, do not assume primary identity or DNS stability; verify image IDs if agents behave stale.
- [ ] **Step 2:** Write the user-guide section.
- [ ] **Step 3:** Commit: `test(e2e): pooler flow — auth_query, pooled failover, knob roll, disable` and `docs(user-guide): connection pooling section`

---

## Self-review notes

- Spec coverage: spec block (T1), ini/userlist/TLS lines (T2), objects + hash + TLS mount (T3), deps key (T4), agent ensure (T5), reconcile incl. delete-on-disable + managed-secret key backfill (T6), rotation (T7), API/contract (T8), UI (T9), RBAC both paths + preflight (T10), e2e + docs (T11). Failover needs no task (rides `-rw`). Error-handling table rows map to T6 (missing key → warn + skip), T8 (422 codes), deps/required-secrets (T4).
- Preflight (T10) was not in the spec's RBAC section explicitly but `preflight.rs` checks RBAC via SelfSubjectAccessReview — skipping it would make preflight lie. In scope.
- Type consistency: `PoolerSpec`/`PoolMode`/`PoolerStatus`, `pooler_deployment_name`/`pool_service_name`/`pooler_config_secret_name`, `POOLER_PASSWORD_KEY` used identically across T1–T8.
