# Declarative Extensions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Declared extensions on the cluster spec: `CREATE EXTENSION` converged per target database plus `shared_preload_libraries` rendered from flagged entries, per `docs/superpowers/specs/2026-08-04-extensions-design.md`.

**Architecture:** Extends the provisioning pipeline end to end: `ExtensionSpec` in `pglord-core::provision` (validation + SQL builders + preload render), agent applies DDL per distinct target database and appends the preload line to the managed parameter file, reconciler passes extensions through `/provision/apply`, UI extends the provisioning card. `shared_preload_libraries` becomes RESERVED.

**Tech Stack:** existing stack; no new dependencies.

## Global Constraints

- Leave-in-place for DDL (never DROP/ALTER an existing extension); the preload *list* is fully declarative.
- Identifier charset = the roles charset (injection contract); `database` must be a declared database or `postgres`; `plpgsql` reserved.
- Agent re-validates the full spec (defense in depth, like roles).
- Contract + CRD regenerated together (`make openapi ui-client crd`).
- Clippy `-D warnings`; e2e harness pitfalls memory applies to the new script.

---

### Task 1: Core — ExtensionSpec, validation, SQL, preload render, RESERVED

**Files:**
- Modify: `crates/pglord-core/src/provision.rs`, `crates/pglord-core/src/spec.rs` (field + SpecChange + validate call passes extensions), `crates/pglord-core/src/guc.rs` (RESERVED)

**Interfaces (produced):**

```rust
// provision.rs
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[cfg_attr(feature = "crd", derive(schemars::JsonSchema))]
pub struct ExtensionSpec {
    /// Strict lowercase identifier — the injection contract.
    pub name: String,
    /// Target database: a declared database or "postgres".
    pub database: String,
    /// Optional schema to install into (same charset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Also load the library at server start (shared_preload_libraries).
    /// Restart-required; surfaces through pending_restart.
    #[serde(default)]
    pub preload: bool,
}

pub fn extension_ddl(ext: &ExtensionSpec) -> String;          // CREATE EXTENSION IF NOT EXISTS "x" [SCHEMA "s"]
pub fn preload_libraries(extensions: &[ExtensionSpec]) -> Option<String>; // sorted dedup csv; None when no preload entries
// validate(roles, databases, extensions) — signature gains the third slice
```

- [ ] **Step 1: Failing tests** in `provision.rs` tests (follow the existing validation-test shapes):

```rust
    #[test]
    fn extension_validation_rules() {
        let dbs = vec![DatabaseSpec { name: "appdb".into(), owner: "app".into() }];
        let ok = ExtensionSpec { name: "pgcrypto".into(), database: "appdb".into(), schema: None, preload: false };
        let in_postgres = ExtensionSpec { name: "pg_stat_statements".into(), database: "postgres".into(), schema: None, preload: true };
        assert!(validate_extensions(&[ok.clone(), in_postgres], &dbs).is_ok());
        // unknown database
        let bad_db = ExtensionSpec { database: "nope".into(), ..ok.clone() };
        assert!(validate_extensions(&[bad_db], &dbs).is_err());
        // charset
        let bad_name = ExtensionSpec { name: "pg;drop".into(), ..ok.clone() };
        assert!(validate_extensions(&[bad_name], &dbs).is_err());
        // duplicate (name, database)
        assert!(validate_extensions(&[ok.clone(), ok.clone()], &dbs).is_err());
        // built-in
        let plpgsql = ExtensionSpec { name: "plpgsql".into(), ..ok.clone() };
        assert!(validate_extensions(&[plpgsql], &dbs).is_err());
    }

    #[test]
    fn extension_ddl_shapes() {
        let plain = ExtensionSpec { name: "pgcrypto".into(), database: "appdb".into(), schema: None, preload: false };
        assert_eq!(extension_ddl(&plain), r#"CREATE EXTENSION IF NOT EXISTS "pgcrypto""#);
        let schemad = ExtensionSpec { schema: Some("gis".into()), name: "postgis".into(), ..plain };
        assert_eq!(extension_ddl(&schemad), r#"CREATE EXTENSION IF NOT EXISTS "postgis" SCHEMA "gis""#);
    }

    #[test]
    fn preload_line_is_sorted_deduped_and_absent_when_empty() {
        let e = |n: &str, p: bool| ExtensionSpec { name: n.into(), database: "postgres".into(), schema: None, preload: p };
        assert_eq!(preload_libraries(&[e("timescaledb", true), e("pg_stat_statements", true), e("pgcrypto", false), e("timescaledb", true)]),
                   Some("pg_stat_statements,timescaledb".into()));
        assert_eq!(preload_libraries(&[e("pgcrypto", false)]), None);
    }
```

Plus in `guc.rs` tests: `assert!(is_reserved("shared_preload_libraries"));` and in `spec.rs` tests an end-to-end validate case (extension on undeclared db rejected through `ClusterSpec::validate`, SpecChange full-replacement passthrough like the roles test).

- [ ] **Step 2:** `cargo test -p pglord-core extension` → FAIL.
- [ ] **Step 3:** Implement: `ExtensionSpec`; `validate_extensions(exts, databases) -> Result<(), ProvisionError>` (new `ProvisionError` variants: `InvalidExtensionName(String)`, `UnknownExtensionDatabase { extension: String, database: String }`, `DuplicateExtension { name: String, database: String }`, `ReservedExtension(String)`); keep the public `validate(roles, dbs)` and add `validate_all(roles, dbs, exts)` (spec.rs calls `validate_all`; the agent's existing call sites migrate too — grep `provision::validate(`). `extension_ddl`, `preload_libraries` (BTreeSet for sort+dedup). `spec.rs`: `pub extensions: Vec<crate::provision::ExtensionSpec>` (serde default, doc comment) + `SpecChange.extensions: Option<Vec<...>>` with the roles replacement arm + restore.rs `restored_spec` carries `extensions: source.extensions.clone()` (same rationale as roles). guc.rs RESERVED: `"shared_preload_libraries"` with comment "driven by the extensions block; a hand-set list would fight the render".
- [ ] **Step 4:** `cargo test -p pglord-core && cargo test -p pglord-core --features crd` → PASS (compiler will list every `ClusterSpec` literal and `validate(` call needing the new field/signature — fix each).
- [ ] **Step 5:** Commit `feat(core): declared extensions — validation, ddl builders, preload render, reserved guc`.

---

### Task 2: Agent — per-database DDL + preload line

**Files:**
- Modify: `crates/pglord-agent/src/postboot.rs` (DTO + apply), `crates/pglord-agent/src/http.rs:223-` (validate call), `crates/pglord-agent/src/record.rs:174` area (preload append)

**Interfaces:**
- Consumes: `extension_ddl`, `preload_libraries`, `validate_all` (Task 1).
- Produces: `ProvisionRequest.extensions: Vec<ProvisionExtension>`; `connect_local_db(cfg, db)`.

- [ ] **Step 1:** DTO next to `ProvisionDatabase`:

```rust
#[derive(serde::Deserialize)]
pub struct ProvisionExtension {
    pub name: String,
    pub database: String,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub preload: bool,
}
```

`ProvisionRequest` gains `#[serde(default)] pub extensions: Vec<ProvisionExtension>`; `to_specs` returns the triple and maps it to `pglord_core::provision::ExtensionSpec`.
- [ ] **Step 2:** Generalize the connector: `connect_local` becomes `connect_local_db(cfg, dbname: &str)` with the old name kept as `connect_local(cfg) = connect_local_db(cfg, "postgres")` (grep other `connect_local` callers — status.rs has its own path; only postboot's matter).
- [ ] **Step 3:** In `apply_provisioning`, after databases, group `req.extensions` by database and per distinct database open a `connect_local_db` connection, then for each extension: existence check `SELECT EXISTS(SELECT 1 FROM pg_extension WHERE extname = $1)` and `batch_execute(&extension_ddl(spec))` only when absent. A failing database logs and continues with the others (collect errors, return the first at the end so the reconciler retries):

```rust
    let mut first_err: Option<anyhow::Error> = None;
    let mut by_db: std::collections::BTreeMap<&str, Vec<&pglord_core::provision::ExtensionSpec>> = Default::default();
    for spec in &ext_specs {
        by_db.entry(spec.database.as_str()).or_default().push(spec);
    }
    for (db, exts) in by_db {
        match apply_extensions_in(cfg, db, &exts).await {
            Ok(()) => {}
            Err(err) => {
                tracing::warn!(%db, %err, "extension ensure failed in database; continuing");
                first_err.get_or_insert(err);
            }
        }
    }
    if let Some(err) = first_err {
        return Err(err);
    }
```

- [ ] **Step 4:** Preload: in `record.rs` `apply_parameters` where `render_parameters` output is written, append the preload line before `write_if_changed`:

```rust
    let mut rendered = pglord_core::guc::render_parameters(&spec.parameters);
    if let Some(libs) = pglord_core::provision::preload_libraries(&spec.extensions) {
        if rendered.is_empty() {
            rendered.push_str("# Managed by pglord. Edited through the pglord API/UI.\n");
        }
        rendered.push_str(&format!("shared_preload_libraries = '{libs}'\n"));
    }
```

(Also check `main.rs`/bootstrap for a first-boot parameter write — grep `render_parameters` across the agent; every call site gets the same append, extracted as a small helper `params_with_preload(spec) -> String` in the agent if there is more than one.)
- [ ] **Step 5:** http.rs handler: validation switches to `validate_all(&role_specs, &db_specs, &ext_specs)`.
- [ ] **Step 6:** Tests: `to_specs` mapping test; `params_with_preload` renders the quoted line and omits it with no preload entries. `cargo test -p pglord-agent && make lint` → PASS.
- [ ] **Step 7:** Commit `feat(agent): create extensions per target database; render shared_preload_libraries`.

---

### Task 3: Server passthrough

**Files:**
- Modify: `crates/pglord-server/src/reconciler.rs:480,513-535`, `crates/pglord-server/src/api/clusters.rs` (DTO + detail fill)

- [ ] **Step 1:** `apply_provisioning` gate becomes `spec.roles.is_empty() && spec.databases.is_empty() && spec.extensions.is_empty()`; payload json gains:

```rust
    let extensions: Vec<_> = spec
        .extensions
        .iter()
        .map(|e| serde_json::json!({
            "name": e.name, "database": e.database,
            "schema": e.schema, "preload": e.preload,
        }))
        .collect();
```

and `"extensions": extensions` in the POST body.
- [ ] **Step 2:** `ClusterDetail` gains `pub extensions: Vec<pglord_core::provision::ExtensionSpec>` (doc: "Declared extensions; leave-in-place like roles"), filled from the spec in `get_cluster`.
- [ ] **Step 3:** `cargo test -p pglord-server && make lint` → PASS. Commit `feat(server): extensions through provisioning apply and the cluster detail`.

---

### Task 4: Contract + CRD regen

- [ ] `make openapi ui-client crd` → commit regenerated `spec/openapi.yaml`, `ui/src/app/api/**`, `deploy/crds/postgresclusters.pglord.dev.yaml`; `make openapi-check ui-client-check crd-check` clean post-commit. Commit `chore(contract): regenerate openapi + ts client + crd for extensions`.

---

### Task 5: UI — provisioning card extensions

**Files:**
- Modify: `ui/src/app/pages/clusters/cluster-detail.component.ts` (card title + extensions table), `ui/src/app/pages/clusters/cluster-provision-dialog.component.ts` (extension rows + result), `ui/public/i18n/en.json`

- [ ] **Step 1:** Dialog: `ProvisionResult` gains `extensions: ExtensionSpec[]`; a third row-editor section (name, database select over declared databases + `postgres`, schema input, preload checkbox), following the existing role/database row idioms; validity = same NAME_RE charset.
- [ ] **Step 2:** Detail card: retitle via i18n `provision.title` = "Provisioning"; add an extensions table (name / database / schema / preload badge) shown when `d.extensions.length`; the save handler patches `{ roles, databases, extensions }`.
- [ ] **Step 3:** i18n keys: `provision.extensions`, `.extensionName`, `.extensionDatabase`, `.extensionSchema`, `.extensionPreload`, `.preloadHint` ("Requires a restart — watch for the pending-restart banner").
- [ ] **Step 4:** `make ui-lint ui-build ui-test` → clean. Commit `feat(ui): extensions in the provisioning card and editor`.

---

### Task 6: E2E + docs

**Files:**
- Create: `scripts/extensions-e2e.sh`
- Modify: `Makefile`, `docs/user-guide.md`

- [ ] **Step 1:** Script (hibernate-e2e harness, port 18100, standalone configmap backend is fine — provisioning is backend-agnostic):
  1. create cluster `demo` (replicas 2) with `databases: [{name: appdb, owner: app_rw}]`, `roles: [{name: app_rw}]`;
  2. PATCH `extensions: [{name: pgcrypto, database: appdb}]` → poll until `psql -d appdb -c "SELECT digest('x','sha256')"` works on the primary AND the same call in `postgres` db fails (per-database scoping);
  3. PATCH adding `{name: pg_stat_statements, database: postgres, preload: true}` → detail API shows `pending_restart: true` → `POST /restart` → poll ready → `SELECT count(*) FROM pg_stat_statements` works; `SHOW shared_preload_libraries` contains it;
  4. PATCH removing the pgcrypto entry → digest() still works in appdb (leave-in-place);
  5. PATCH removing the preload entry → restart → `SHOW shared_preload_libraries` empty;
  6. delete cluster.
- [ ] **Step 2:** Makefile `extensions-e2e` target; run on the stand.
- [ ] **Step 3:** User guide: extensions subsection under provisioning — spec example, preload/restart flow, image-must-ship-the-extension note, missing-preload-library recovery, leave-in-place semantics; init-scripts section pointer updated.
- [ ] **Step 4:** Commit `test(e2e): extensions flow` and `docs(user-guide): declared extensions`.

---

## Self-review notes

- Spec coverage: spec surface/validation → T1; preload render + RESERVED → T1/T2; per-db DDL + defense-in-depth validate → T2; reconciler payload + detail → T3; contract/CRD → T4; UI → T5; e2e (incl. per-db scoping, preload lifecycle, leave-in-place) + docs → T6.
- Type consistency: `ExtensionSpec` fields used identically in agent DTO, reconciler json, UI dialog; `validate_all` introduced T1, adopted T2; `preload_libraries` consumed only by the agent (T2).
- The `validate` → `validate_all` migration keeps the old fn for any external callers found by grep; if none remain, delete it in T1 (compiler decides).
