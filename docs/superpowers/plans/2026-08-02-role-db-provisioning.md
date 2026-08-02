# Role/Database Provisioning Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `roles:` and `databases:` lists on the cluster spec that pglord converges level-triggered — passwords from referenced Secrets, additive semantics (nothing is ever dropped).

**Architecture:** New `provision` module in `pglord-core` (spec types, validation, pure DDL builders); agent `POST /provision/apply` (provably-primary, idempotent, mirrors `/pooler/ensure`); reconciler `apply_provisioning` (mirrors `apply_pooler`: resolve Secrets → POST → warn-and-retry). Spec: `docs/superpowers/specs/2026-08-02-role-db-provisioning-design.md`.

**Tech Stack:** Rust (axum, kube-rs, tokio-postgres), Angular + spartan hlm + Transloco.

## Global Constraints

- Rust edition 2024; clippy denies warnings and `unwrap_used` — `expect` only in tests.
- Never hand-edit `spec/openapi.yaml` or `ui/src/app/api/**`; run `make openapi ui-client` after API changes, commit artifacts with the code.
- UI strings via Transloco (`ui/public/i18n/en.json`).
- Passwords never appear in the spec, the detail DTO, or logs — Secret references only; values move reconciler→agent over pod HTTP like rotation.
- Verify per task: `cargo test -p <crate>`, `make lint`; commit style `feat(core|agent|server|ui)`, `chore(contract)`, `test(e2e)`, `docs(user-guide)`.

---

### Task 1: `provision` spec types + validation + spec wiring

**Files:**
- Create: `crates/pglord-core/src/provision.rs`
- Modify: `crates/pglord-core/src/lib.rs` (`pub mod provision;`)
- Modify: `crates/pglord-core/src/spec.rs` (fields, `SpecChange`, `validate`, `SpecError::InvalidProvision`)
- Modify: `crates/pglord-server/src/api/restore.rs` (`restored_spec` carries both lists)

**Interfaces:**
- Produces:

```rust
// provision.rs
pub struct RoleSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_secret: Option<crate::env::SecretKeyRefSpec>,
    #[serde(default = "default_true")]
    pub login: bool,
    #[serde(default)]
    pub createdb: bool,
    #[serde(default)]
    pub createrole: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_limit: Option<i32>,
    #[serde(default)]
    pub in_roles: Vec<String>,
}
pub struct DatabaseSpec { pub name: String, pub owner: String }
pub enum ProvisionError {
    InvalidName(String), ReservedName(String), DuplicateName(String),
    UnknownRole { referrer: String, role: String },
    InvalidSecretRef { role: String },
    InvalidConnectionLimit { role: String, limit: i32 },
}
pub fn validate(roles: &[RoleSpec], databases: &[DatabaseSpec]) -> Result<(), ProvisionError>;
pub fn is_valid_pg_name(s: &str) -> bool;   // [a-z_][a-z0-9_]*, len<=63
```

  Derives on both structs: `Debug, Clone, Default (DatabaseSpec no Default), PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema`. Reserved role names: `postgres`, `replicator`, `pglord_pooler`, prefix `pg_`; reserved database names additionally `template0`, `template1`. `connection_limit` must be `>= -1`.
- `ClusterSpec` gains `#[serde(default)] pub roles: Vec<crate::provision::RoleSpec>` and `#[serde(default)] pub databases: Vec<crate::provision::DatabaseSpec>`; `SpecChange` gains both as `Option<Vec<…>>` (pg_hba semantics: absent untouched, present full replacement); `validate` calls `provision::validate(&self.roles, &self.databases)`; `SpecError` gains `#[error("{0}")] InvalidProvision(#[from] crate::provision::ProvisionError)`. `env::SecretKeyRefSpec` needs `PartialEq, Eq` added if missing.

- [ ] **Step 1: failing tests** in `provision.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn role(name: &str) -> RoleSpec {
        serde_json::from_value(serde_json::json!({ "name": name })).expect("role")
    }
    #[test]
    fn defaults_are_login_only() {
        let r = role("app_rw");
        assert!(r.login && !r.createdb && !r.createrole);
        assert!(r.password_secret.is_none() && r.in_roles.is_empty());
    }
    #[test]
    fn names_are_strict_identifiers() {
        for bad in ["", "App", "1st", "has-dash", "has space", &"x".repeat(64)] {
            assert!(matches!(
                validate(&[role(bad)], &[]),
                Err(ProvisionError::InvalidName(_))
            ), "{bad:?} accepted");
        }
        assert!(validate(&[role("app_rw2")], &[]).is_ok());
    }
    #[test]
    fn reserved_names_are_rejected() {
        for bad in ["postgres", "replicator", "pglord_pooler", "pg_monitor"] {
            assert!(matches!(
                validate(&[role(bad)], &[]),
                Err(ProvisionError::ReservedName(_))
            ), "{bad} accepted");
        }
        for bad in ["postgres", "template0", "template1"] {
            let db = DatabaseSpec { name: bad.into(), owner: "app_rw".into() };
            assert!(matches!(
                validate(&[role("app_rw")], &[db]),
                Err(ProvisionError::ReservedName(_))
            ), "db {bad} accepted");
        }
    }
    #[test]
    fn references_must_name_declared_roles() {
        let mut r = role("app_rw");
        r.in_roles = vec!["reporting".into()];
        assert!(matches!(
            validate(&[r], &[]),
            Err(ProvisionError::UnknownRole { .. })
        ));
        let db = DatabaseSpec { name: "appdb".into(), owner: "ghost".into() };
        assert!(matches!(
            validate(&[role("app_rw")], &[db]),
            Err(ProvisionError::UnknownRole { .. })
        ));
    }
    #[test]
    fn duplicates_and_bad_limits_are_rejected() {
        assert!(matches!(
            validate(&[role("a"), role("a")], &[]),
            Err(ProvisionError::DuplicateName(_))
        ));
        let mut r = role("a");
        r.connection_limit = Some(-2);
        assert!(matches!(
            validate(&[r], &[]),
            Err(ProvisionError::InvalidConnectionLimit { .. })
        ));
    }
}
```

  plus in `spec.rs` tests:

```rust
#[test]
fn provision_lists_replace_wholesale() {
    let base = spec("demo");
    let change: SpecChange = serde_json::from_value(serde_json::json!({
        "roles": [{ "name": "app_rw" }],
        "databases": [{ "name": "appdb", "owner": "app_rw" }]
    }))
    .expect("change");
    let with = base.apply_change(&change).expect("apply");
    assert_eq!(with.roles.len(), 1);
    assert_eq!(with.databases[0].owner, "app_rw");
    // Absent leaves untouched; empty lists stop managing.
    assert_eq!(with.apply_change(&SpecChange::default()).expect("a").roles.len(), 1);
    let clear: SpecChange =
        serde_json::from_value(serde_json::json!({ "roles": [], "databases": [] })).expect("c");
    assert!(with.apply_change(&clear).expect("a").roles.is_empty());
    // Validation runs: a database owned by an undeclared role is rejected.
    let bad: SpecChange = serde_json::from_value(serde_json::json!({
        "databases": [{ "name": "appdb", "owner": "ghost" }]
    }))
    .expect("change");
    assert!(base.apply_change(&bad).is_err());
}
```

- [ ] **Step 2:** `cargo test -p pglord-core provision` → FAIL (module missing)
- [ ] **Step 3:** Implement; wire spec.rs arms (`if let Some(roles) = &change.roles { next.roles = roles.clone(); }`, same for databases); add both lists to `restored_spec` in `api/restore.rs` with a comment ("the restored data already contains these; convergence is a no-op or fixes drift").
- [ ] **Step 4:** `cargo test --workspace` → PASS; `cargo fmt --all && make lint`
- [ ] **Step 5:** Commit: `feat(core): role and database spec lists with strict identifier validation`

---

### Task 2: pure DDL builders

**Files:**
- Modify: `crates/pglord-core/src/provision.rs`

**Interfaces:**
- Produces (all pure; identifiers pre-validated by Task 1, passwords by the agent's `is_safe_password`):

```rust
/// CREATE or ALTER for one role. `password: Some` appends the PASSWORD clause
/// (dollar-quoted with $pglord$); None leaves any existing password untouched.
pub fn role_ddl(role: &RoleSpec, exists: bool, password: Option<&str>) -> String;
/// (grants, revokes) of (member, group) pairs: desired minus current, and
/// current-among-declared minus desired. `current` = memberships where BOTH
/// sides are declared roles.
pub fn membership_changes(
    roles: &[RoleSpec],
    current: &[(String, String)],
) -> (Vec<(String, String)>, Vec<(String, String)>);
pub fn grant_sql(member: &str, group: &str) -> String;   // GRANT "g" TO "m"
pub fn revoke_sql(member: &str, group: &str) -> String;  // REVOKE "g" FROM "m"
/// None when nothing to do; CREATE when absent, ALTER OWNER when owner differs.
pub fn database_ddl(db: &DatabaseSpec, current_owner: Option<&str>) -> Option<String>;
```

- [ ] **Step 1: failing tests:**

```rust
#[test]
fn role_ddl_renders_all_attributes() {
    let r: RoleSpec = serde_json::from_value(serde_json::json!({
        "name": "app_rw", "createdb": true, "connection_limit": 5
    }))
    .expect("role");
    let ddl = role_ddl(&r, false, Some("pw-1"));
    assert!(ddl.starts_with("CREATE ROLE \"app_rw\""));
    assert!(ddl.contains("LOGIN"));
    assert!(ddl.contains("CREATEDB"));
    assert!(ddl.contains("NOCREATEROLE"));
    assert!(ddl.contains("CONNECTION LIMIT 5"));
    assert!(ddl.contains("PASSWORD $pglord$pw-1$pglord$"));
    let alter = role_ddl(&r, true, None);
    assert!(alter.starts_with("ALTER ROLE \"app_rw\""));
    assert!(!alter.contains("PASSWORD"), "no password clause without a value");
    // Absent limit renders postgres's own "no limit".
    let r2: RoleSpec = serde_json::from_value(serde_json::json!({ "name": "a", "login": false }))
        .expect("role");
    let ddl2 = role_ddl(&r2, false, None);
    assert!(ddl2.contains("NOLOGIN"));
    assert!(ddl2.contains("CONNECTION LIMIT -1"));
}

#[test]
fn membership_diff_grants_and_revokes_only_among_declared() {
    let roles: Vec<RoleSpec> = serde_json::from_value(serde_json::json!([
        { "name": "reporting" },
        { "name": "app_rw", "in_roles": ["reporting"] },
        { "name": "app_ro" }
    ]))
    .expect("roles");
    // current: app_ro is in reporting (stale), app_rw not yet granted.
    let current = vec![("app_ro".to_string(), "reporting".to_string())];
    let (grants, revokes) = membership_changes(&roles, &current);
    assert_eq!(grants, vec![("app_rw".to_string(), "reporting".to_string())]);
    assert_eq!(revokes, vec![("app_ro".to_string(), "reporting".to_string())]);
    assert_eq!(grant_sql("app_rw", "reporting"), "GRANT \"reporting\" TO \"app_rw\"");
    assert_eq!(revoke_sql("app_ro", "reporting"), "REVOKE \"reporting\" FROM \"app_ro\"");
}

#[test]
fn database_ddl_creates_then_converges_owner() {
    let db = DatabaseSpec { name: "appdb".into(), owner: "app_rw".into() };
    assert_eq!(
        database_ddl(&db, None).as_deref(),
        Some("CREATE DATABASE \"appdb\" OWNER \"app_rw\"")
    );
    assert_eq!(
        database_ddl(&db, Some("old_owner")).as_deref(),
        Some("ALTER DATABASE \"appdb\" OWNER TO \"app_rw\"")
    );
    assert_eq!(database_ddl(&db, Some("app_rw")), None, "converged is a no-op");
}
```

- [ ] **Step 2:** `cargo test -p pglord-core provision` → FAIL
- [ ] **Step 3:** Implement (plain `format!` construction; `membership_changes` builds desired/current `BTreeSet<(String,String)>`s and diffs them, output sorted for determinism).
- [ ] **Step 4:** `cargo test -p pglord-core` → PASS; `make lint`
- [ ] **Step 5:** Commit: `feat(core): pure ddl builders for role, membership and database convergence`

---

### Task 3: deps.rs — `RolePassword` purpose

**Files:**
- Modify: `crates/pglord-core/src/deps.rs`

**Interfaces:**
- Produces: `ResourcePurpose::RolePassword` (doc: "Password for a declared role."); in `required_resources`, after the env loop:

```rust
for role in &spec.roles {
    if let Some(secret) = &role.password_secret {
        push(RequiredResource {
            name: secret.name.clone(),
            kind: ResourceKind::Secret,
            purpose: ResourcePurpose::RolePassword,
            keys: vec![secret.key.clone()],
            managed: false,
        });
    }
}
```

- [ ] **Step 1: failing test:**

```rust
#[test]
fn declared_role_password_secrets_are_required() {
    let r = required_resources(&spec(serde_json::json!({
        "name": "demo",
        "roles": [
            { "name": "app_rw", "password_secret": { "name": "app-creds", "key": "password" } },
            { "name": "app_ro" }
        ]
    })));
    let creds = r.iter().find(|x| x.name == "app-creds").expect("app-creds");
    assert_eq!(creds.purpose, ResourcePurpose::RolePassword);
    assert_eq!(creds.keys, vec!["password"]);
    assert!(!creds.managed);
    // A role without a reference demands nothing.
    assert_eq!(r.iter().filter(|x| x.purpose == ResourcePurpose::RolePassword).count(), 1);
}
```

- [ ] **Step 2:** run → FAIL; **Step 3:** implement; **Step 4:** `cargo test -p pglord-core` PASS + `make lint`
- [ ] **Step 5:** Commit: `feat(core): declared role password secrets in required resources`

---

### Task 4: agent `POST /provision/apply`

**Files:**
- Modify: `crates/pglord-agent/src/postboot.rs` (`apply_provisioning` + current-state queries)
- Modify: `crates/pglord-agent/src/http.rs` (route + handler + guard tests)

**Interfaces:**
- Request body (`serde::Deserialize` structs local to http.rs):

```rust
struct ProvisionRequest { roles: Vec<ProvisionRole>, databases: Vec<ProvisionDatabase> }
struct ProvisionRole {
    name: String,
    #[serde(default)] password: Option<String>,
    #[serde(default = "default_true")] login: bool,
    #[serde(default)] createdb: bool,
    #[serde(default)] createrole: bool,
    #[serde(default)] connection_limit: Option<i32>,
    #[serde(default)] in_roles: Vec<String>,
}
struct ProvisionDatabase { name: String, owner: String }
```

  Handler: 400 when any name fails `pglord_core::provision::is_valid_pg_name` or any password fails `is_safe_password`; 409 when `in_recovery != Some(false)`; else `postboot::apply_provisioning(&cfg, &req).await` → 200 `{"status":"applied"}` / 500 `{"error": …}`.
- `postboot::apply_provisioning`: one `connect_local` connection; per role query `SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = $1)` then `batch_execute(role_ddl(...))` (rebuild a `pglord_core::provision::RoleSpec` from the request fields, `password_secret: None`); memberships: query current pairs among declared roles

```sql
SELECT m.rolname, g.rolname
FROM pg_auth_members am
JOIN pg_roles m ON m.oid = am.member
JOIN pg_roles g ON g.oid = am.roleid
WHERE m.rolname = ANY($1) AND g.rolname = ANY($1)
```

  then apply `membership_changes` → `grant_sql`/`revoke_sql`; per database query `SELECT r.rolname FROM pg_database d JOIN pg_roles r ON r.oid = d.datdba WHERE d.datname = $1` (`Option<String>`) then `batch_execute` the `database_ddl` when `Some`.

- [ ] **Step 1: failing guard tests** (http.rs, mirror the `/pooler/ensure` pair):

```rust
#[tokio::test]
async fn provision_rejects_unsafe_input_without_touching_postgres() {
    let resp = test_router()
        .oneshot(
            Request::post("/provision/apply")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"roles":[{"name":"app_rw","password":"has$pglord$end"}],"databases":[]}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = test_router()
        .oneshot(
            Request::post("/provision/apply")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"roles":[{"name":"Bad-Name"}],"databases":[]}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn provision_refuses_when_not_provably_primary() {
    let resp = test_router()
        .oneshot(
            Request::post("/provision/apply")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"roles":[{"name":"app_rw"}],"databases":[]}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}
```

- [ ] **Step 2:** `cargo test -p pglord-agent` → FAIL (404)
- [ ] **Step 3:** Implement route `.route("/provision/apply", post(apply_provisioning_handler))`, handler + postboot fn as above.
- [ ] **Step 4:** `cargo test -p pglord-agent` → PASS; `make lint`
- [ ] **Step 5:** Commit: `feat(agent): POST /provision/apply — converge declared roles and databases`

---

### Task 5: reconciler `apply_provisioning`

**Files:**
- Modify: `crates/pglord-server/src/reconciler.rs`

**Interfaces:**
- Consumes: agent endpoint (Task 4), `spec.roles`/`spec.databases`.
- Produces: `async fn apply_provisioning(client, http, ns, spec, primary_pod) -> anyhow::Result<()>` called at the end of `apply_cluster` (after `apply_pooler`). Behavior:
  - return Ok immediately when both lists are empty;
  - for each role with a `password_secret`, read the Secret key (`Api<Secret>::get_opt`); missing Secret/key/non-UTF8 → `tracing::warn!` and return Ok (skip the pass — required-secrets is the surface);
  - build the JSON body (role fields + resolved `password`), POST `http://{pod_fqdn(primary_pod, &spec.name, ns)}:{AGENT_HTTP_PORT}/provision/apply`;
  - non-2xx / connect error → `tracing::warn!("provisioning ensure refused/unreachable; retrying next pass")`, still Ok (level-triggered).

- [ ] **Step 1:** No new pure logic to unit-test (resolution + HTTP, matching `apply_pooler`'s tested-by-e2e stance); write the code:

```rust
async fn apply_provisioning(
    client: &kube::Client,
    http: &reqwest::Client,
    ns: &str,
    spec: &ClusterSpec,
    primary_pod: &str,
) -> anyhow::Result<()> {
    if spec.roles.is_empty() && spec.databases.is_empty() {
        return Ok(());
    }
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let mut roles = Vec::with_capacity(spec.roles.len());
    for role in &spec.roles {
        let password = match &role.password_secret {
            None => None,
            Some(sref) => {
                let value = secrets
                    .get_opt(&sref.name)
                    .await?
                    .and_then(|s| s.data)
                    .and_then(|d| d.get(&sref.key).cloned())
                    .and_then(|v| String::from_utf8(v.0).ok());
                match value {
                    Some(v) => Some(v),
                    None => {
                        tracing::warn!(
                            cluster = %spec.name, role = %role.name, secret = %sref.name,
                            key = %sref.key,
                            "role password secret unusable; provisioning skipped this pass"
                        );
                        return Ok(());
                    }
                }
            }
        };
        roles.push(serde_json::json!({
            "name": role.name,
            "password": password,
            "login": role.login,
            "createdb": role.createdb,
            "createrole": role.createrole,
            "connection_limit": role.connection_limit,
            "in_roles": role.in_roles,
        }));
    }
    let databases: Vec<_> = spec
        .databases
        .iter()
        .map(|d| serde_json::json!({ "name": d.name, "owner": d.owner }))
        .collect();
    let url = format!(
        "http://{}:{}/provision/apply",
        pod_fqdn(primary_pod, &spec.name, ns),
        AGENT_HTTP_PORT
    );
    match http
        .post(&url)
        .json(&serde_json::json!({ "roles": roles, "databases": databases }))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => tracing::warn!(
            cluster = %spec.name, status = %resp.status(),
            "provisioning ensure refused; retrying next pass"
        ),
        Err(err) => tracing::warn!(
            cluster = %spec.name, %err,
            "provisioning ensure unreachable; retrying next pass"
        ),
    }
    Ok(())
}
```

  and `apply_provisioning(client, http, ns, spec, primary_pod).await?;` as the last line of `apply_cluster` before `Ok(())`.
- [ ] **Step 2:** `cargo test --workspace` → PASS; `make lint`
- [ ] **Step 3:** Commit: `feat(server): level-triggered role and database provisioning via the primary agent`

---

### Task 6: cluster detail + contract regen

**Files:**
- Modify: `crates/pglord-server/src/api/clusters.rs`
- Regenerate: `spec/openapi.yaml`, `ui/src/app/api/**` (via make only)

**Interfaces:**
- `ClusterDetail` gains

```rust
/// Declared roles (references only — never password values).
pub roles: Vec<pglord_core::provision::RoleSpec>,
/// Declared databases.
pub databases: Vec<pglord_core::provision::DatabaseSpec>,
```

  populated from `spec.roles` / `spec.databases` in `get_cluster`. `spec_error` gains `SpecError::InvalidProvision(e) => match e { ProvisionError::UnknownRole{..} if …` — keep it simple: one arm `SpecError::InvalidProvision(pglord_core::provision::ProvisionError::ReservedName(_) | …)`? No: map ALL `InvalidProvision` variants that mention databases to nothing special — single code per entity is enough:

```rust
SpecError::InvalidProvision(_) => "invalid_provision",
```

  (one stable code; the `detail` string carries the specific variant message — same fidelity the workload/backup codes give).
- [ ] **Step 1:** Add fields + populate + error arm; `cargo test --workspace` → PASS; `make lint`
- [ ] **Step 2:** `make openapi ui-client` then `make openapi-check ui-client-check` → clean after commit
- [ ] **Step 3:** Commit code: `feat(server): declared roles and databases on the cluster detail`
- [ ] **Step 4:** Commit artifacts: `chore(contract): regenerate openapi + ts client for provisioning`

---

### Task 7: UI — roles & databases card + editor

**Files:**
- Create: `ui/src/app/pages/clusters/cluster-provision-dialog.component.ts`
- Modify: `ui/src/app/pages/clusters/cluster-detail.component.ts` (card after the pooler card, `openProvision` handler)
- Modify: `ui/public/i18n/en.json`

**Interfaces:**
- Dialog (pattern: env editor's row-list + the pooler dialog's shell): editable role rows (name, login/createdb/createrole checkboxes, connection limit number, secret name+key inputs, comma-separated memberships input) and database rows (name input, owner `<select>` over declared role names). Closes with `{ roles: RoleSpec[], databases: DatabaseSpec[] } | undefined`; the handler PATCHes `{ roles, databases }` via `patchCluster`.
- Card: count summary (`'provision.summary' | transloco: { roles: d.roles.length, databases: d.databases.length }`), per-role line with flags, per-database line with owner; empty state `provision.none`. Icon `lucideUsers` (add to imports + provideIcons).
- i18n keys under `"provision"`: `title` ("Roles & databases"), `edit`, `editTitle`, `explainer` (additive semantics + passwords-from-Secrets, mention removal leaves objects in place), `roles`, `databases`, `roleName`, `login`, `createdb`, `createrole`, `connectionLimit`, `secretName`, `secretKey`, `memberships`, `membershipsHelp`, `dbName`, `owner`, `addRole`, `addDatabase`, `remove`, `apply`, `summary`, `none`.

- [ ] **Step 1:** Implement dialog + card + strings.
- [ ] **Step 2:** `cd ui && npm run lint && npm run build` → clean
- [ ] **Step 3:** Commit: `feat(ui): roles and databases card with row editor`

---

### Task 8: e2e + user guide

**Files:**
- Create: `scripts/provision-e2e.sh` (harness copied from `scripts/pooler-e2e.sh`: image build/load + stale-image verify + port-forward on port 18100, cluster `prov`)
- Modify: `Makefile` (`provision-e2e` target after `pooler-e2e`)
- Modify: `docs/user-guide.md` (section after Connection pooling; init-scripts section gains a pointer)

**E2e flow (each step `echo "==> …"`/`OK`/`FAIL` like the sibling suites):**
1. 422: role named `postgres` rejected; database owned by undeclared role rejected.
2. Create Secret `app-creds` (`kubectl create secret generic app-creds --from-literal=password=app-pw-1`); create cluster `prov` replicas=2, pooler enabled, roles `[reporting (nologin), app_rw (password_secret app-creds/password, in_roles [reporting])]`, databases `[appdb owner app_rw]`.
3. Poll: `psql -h prov-rw -U app_rw -d appdb` with `app-pw-1` succeeds (proves role+password+db+owner); same via `prov-pool` (pooler auth_query composes).
4. `SELECT pg_has_role('app_rw','reporting','member')` on the primary → `t`.
5. PATCH roles replacing `in_roles: []` and `createdb: true`; poll until `pg_has_role` → `f` and `rolcreatedb` → `t`; instance `restartCount`s unchanged (reload-free convergence).
6. PATCH `roles: []`, `databases: []`; role and database still exist (leave-in-place); `app_rw` still connects.
7. Re-declare with `password_secret` pointing at a missing Secret `ghost-creds`; `/api/v1/clusters/prov/secrets` reports `satisfied: false` with `ghost-creds` missing.
8. Point back at `app-creds`, update the Secret to `app-pw-2` (`kubectl create secret generic app-creds --from-literal=password=app-pw-2 --dry-run=client -o yaml | kubectl apply -f -`); poll until psql with `app-pw-2` succeeds and `app-pw-1` is rejected.
9. Delete the primary pod; poll until psql as `app_rw` recovers (ensure survives failover).
10. Cleanup: delete cluster + `app-creds`.

**User guide section covers:** the two lists with a full example, passwords from Secrets (rotation = edit your Secret), additive/leave-in-place semantics, what belongs in init scripts vs provisioning, reserved names, pooler composition.

- [ ] **Step 1:** Write script (+`chmod +x`, `bash -n`), Makefile target, docs; commit: `test(e2e): provisioning flow — converge, leave-in-place, secret rotation, failover` and `docs(user-guide): declarative roles and databases`
- [ ] **Step 2:** `make provision-e2e` on the stand until green (memory notes apply: settle after rolls, re-POST-style retries where an agent call follows pod churn, never bare `wait`).

---

## Self-review notes

- Spec coverage: spec surface + restore carry-over (T1), DDL builders incl. membership diff + database owner convergence (T2), deps (T3), agent endpoint + guards + current-state queries (T4), reconciler resolve+POST+skip semantics (T5), API/contract + 422 code (T6), UI (T7), e2e incl. missing-secret flagging, password rotation via Secret edit, failover, leave-in-place (T8). Error-handling table rows all land in T4/T5/T6/T8. No RBAC change needed (no new object kinds).
- Single `invalid_provision` code instead of the spec's `invalid_role`/`invalid_database` pair — deviation noted deliberately: one enum wraps both entities and the variant message disambiguates; splitting would need two error enums for no client value. Update the spec doc line when committing T6.
- Type consistency: `RoleSpec`/`DatabaseSpec`/`ProvisionError` (T1) used in T2 signatures, T4 request rebuild, T6 DTO; `is_valid_pg_name` shared T1/T4.
