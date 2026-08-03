# PostgresCluster CRD (Operator Mode Stage A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `PostgresCluster` CRD as an alternative spec backend (`PGLORD_SPEC_BACKEND=crd`): CRs are the truth, synced into the existing `<name>-spec` ConfigMap so every downstream loop runs unchanged, per `docs/superpowers/specs/2026-08-03-crd-operator-mode-design.md`.

**Architecture:** CR-fronted, ConfigMap-engined. New: feature-gated CRD types in `pglord-core`, a `CrdSpecStore`, a signal-driven CR-sync loop with status writing, a generated committed CRD manifest, RBAC/preflight additions. Untouched: reconciler, orchestrator, backup scheduler, agent, instance RBAC, API contract, UI.

**Tech Stack:** kube-rs 4.2 (`derive` feature), schemars, serde_yaml (manifest dump), existing axum/utoipa stack.

## Global Constraints

- `configmap` backend stays byte-identical; `crd` mode changes only the store construction and one extra spawned loop.
- CRD spec is `ClusterSpec` verbatim, snake_case field names included.
- `deploy/crds/postgresclusters.pglord.dev.yaml` is generated (`--dump-crd`), committed, drift-checked (`make crd-check`) — never hand-edited.
- The sync never writes `metadata.annotations` on the mirror (the primary record CAS owns them).
- Invalid CRs get status `Invalid` + message; children are never touched by an invalid edit.
- Clippy `-D warnings`, no `unwrap`; `Config::test_default()` must gain any new field.
- Two install paths (`deploy/k8s/`, `charts/pglord/`) change together.

---

### Task 1: Core `crd` feature — CRD types and schema derives

**Files:**
- Modify: `crates/pglord-core/Cargo.toml`, `crates/pglord-core/src/spec.rs`, `crates/pglord-core/src/lib.rs`, every core type reachable from `ClusterSpec` (compiler-driven list)
- Create: `crates/pglord-core/src/crd.rs`

**Interfaces:**
- Produces: feature `crd`; `pglord_core::crd::{PostgresCluster, PostgresClusterStatus}`; `ClusterSpec.name` now `#[serde(default)]`.

- [ ] **Step 1: Dependencies.** In `crates/pglord-core/Cargo.toml`:

```toml
[features]
crd = ["dep:kube", "dep:schemars"]

[dependencies]
kube = { version = "4.2.0", default-features = false, features = ["derive"], optional = true }
schemars = { version = "1", optional = true }
```

Verify the schemars major kube-derive 4.2 expects: `cargo tree -p kube-derive -e normal 2>/dev/null | grep schemars` after a `cargo check -p pglord-core --features crd`; if it pins 0.8, change our dep to `"0.8"`.

- [ ] **Step 2: Failing test** in a new `#[cfg(all(test, feature = "crd"))] mod crd_tests` inside `crates/pglord-core/src/crd.rs` (written together with the module skeleton below):

```rust
#[cfg(all(test, feature = "crd"))]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    #[test]
    fn crd_manifest_has_expected_identity() {
        let crd = PostgresCluster::crd();
        assert_eq!(crd.spec.group, "pglord.dev");
        assert_eq!(crd.spec.names.kind, "PostgresCluster");
        assert_eq!(crd.spec.names.short_names, Some(vec!["pgc".to_string()]));
        let v = &crd.spec.versions[0];
        assert_eq!(v.name, "v1alpha1");
        assert!(v.subresources.as_ref().and_then(|s| s.status.as_ref()).is_some());
        let cols = v.additional_printer_columns.as_ref().expect("printer columns");
        assert!(cols.iter().any(|c| c.json_path == ".status.ready_replicas"));
    }

    #[test]
    fn spec_schema_uses_snake_case_verbatim() {
        let crd = PostgresCluster::crd();
        let schema = serde_json::to_string(&crd.spec.versions[0].schema).expect("schema json");
        assert!(schema.contains("storage_size"));
        assert!(schema.contains("pg_hba"));
    }
}
```

- [ ] **Step 3: Run** `cargo test -p pglord-core --features crd crd_` → FAIL (module missing).
- [ ] **Step 4: Implement.**

`spec.rs` — name default + the CustomResource derive on `ClusterSpec` itself (keeps "verbatim" literal):

```rust
    /// RFC1123 label; used as the prefix of every derived object name.
    /// Empty is invalid through the API; in operator mode the CR sync fills
    /// it from metadata.name, so kubectl users write the name once.
    #[serde(default)]
    pub name: String,
```

On the struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[cfg_attr(
    feature = "crd",
    derive(kube::CustomResource, schemars::JsonSchema),
    kube(
        group = "pglord.dev",
        version = "v1alpha1",
        kind = "PostgresCluster",
        namespaced,
        shortname = "pgc",
        status = "crate::crd::PostgresClusterStatus",
        printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.ready_replicas"}"#,
        printcolumn = r#"{"name":"Primary","type":"string","jsonPath":".status.primary"}"#,
        printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#
    )
)]
pub struct ClusterSpec {
```

(kube-derive generates `PostgresCluster` in the `spec` module.)

`crd.rs`:

```rust
//! Operator-mode CRD surface. The spec type is ClusterSpec verbatim
//! (spec.rs carries the CustomResource derive); this module holds the
//! status subresource and re-exports the generated CR type.

pub use crate::spec::PostgresCluster;

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct PostgresClusterStatus {
    /// Instances currently passing their readiness probe.
    #[serde(default)]
    pub ready_replicas: i32,
    /// Recorded primary pod.
    #[serde(default)]
    pub primary: String,
    /// Scaled to zero on purpose; volumes retained.
    #[serde(default)]
    pub hibernated: bool,
    /// Progressing | Ready | Degraded | Hibernated | Invalid.
    #[serde(default)]
    pub phase: String,
    /// Human-readable detail for Invalid/Degraded; empty otherwise.
    #[serde(default)]
    pub message: String,
}
```

`lib.rs`: `#[cfg(feature = "crd")] pub mod crd;`

- [ ] **Step 5: Compiler-driven JsonSchema derives.** `cargo check -p pglord-core --features crd` and add `#[cfg_attr(feature = "crd", derive(schemars::JsonSchema))]` to every type rustc flags (expected: the ToSchema types in `workload.rs`, `backup.rs`, `pooler.rs`, `tls.rs`, `hba.rs`, `provision.rs`, `env.rs`, `init.rs`, `restore.rs`). Repeat until clean.
- [ ] **Step 6: Guard the REST behavior** — add to `spec.rs` tests:

```rust
    #[test]
    fn empty_name_is_still_invalid() {
        let s: ClusterSpec = serde_json::from_value(serde_json::json!({})).expect("deserialize");
        assert_eq!(s.name, "");
        assert!(matches!(s.validate(), Err(SpecError::InvalidName(_))));
    }
```

- [ ] **Step 7: Verify both worlds.** `cargo test -p pglord-core` (feature off) and `cargo test -p pglord-core --features crd` → PASS; `cargo clippy -p pglord-core --all-targets --features crd -- -D warnings`.
- [ ] **Step 8: Commit** `feat(core): PostgresCluster CRD types behind a crd feature`.

---

### Task 2: `--dump-crd`, committed manifest, drift gate

**Files:**
- Modify: `crates/pglord-server/Cargo.toml` (enable feature + serde_yaml), `crates/pglord-server/src/main.rs:16-23` area, `Makefile`
- Create: `deploy/crds/postgresclusters.pglord.dev.yaml` (generated)

**Interfaces:**
- Consumes: `pglord_core::crd::PostgresCluster` (Task 1).

- [ ] **Step 1:** Server Cargo.toml: `pglord-core = { ..., features = ["crd"] }` (add to the existing dep line) and `serde_yaml = "0.9"`.
- [ ] **Step 2:** In `main.rs`, right after the `--dump-openapi` block:

```rust
    // Early exit: emit the CRD manifest without touching the network.
    if std::env::args().any(|a| a == "--dump-crd") {
        use kube::CustomResourceExt;
        std::fs::create_dir_all("deploy/crds")?;
        let yaml = serde_yaml::to_string(&pglord_core::crd::PostgresCluster::crd())?;
        std::fs::write("deploy/crds/postgresclusters.pglord.dev.yaml", yaml)?;
        println!("wrote deploy/crds/postgresclusters.pglord.dev.yaml");
        return Ok(());
    }
```

- [ ] **Step 3:** Makefile, next to `openapi`/`openapi-check`:

```make
crd: ## Regenerate deploy/crds from code
	cargo run -p pglord-server -- --dump-crd

crd-check: crd ## Fail if the committed CRD manifest drifts from code
	git diff --exit-code deploy/crds
```

- [ ] **Step 4:** `make crd` → manifest written; `kubectl apply --dry-run=client -f deploy/crds/` unavailable offline is fine — instead sanity: `python3 -c "import yaml,sys; yaml.safe_load(open('deploy/crds/postgresclusters.pglord.dev.yaml'))"`.
- [ ] **Step 5:** `make crd-check` after committing → clean. Commit `feat(server): --dump-crd emits the committed PostgresCluster manifest` (include the generated file).

---

### Task 3: `spec_backend` config + `CrdSpecStore` + wiring

**Files:**
- Modify: `crates/pglord-server/src/config.rs` (field + `test_default`), `crates/pglord-server/src/main.rs:79` area, `crates/pglord-server/src/lib.rs`
- Create: `crates/pglord-server/src/crd_store.rs`

**Interfaces:**
- Consumes: `SpecStore` trait (`spec_store.rs:64-76`), `ConfigMapSpecStore` (delegation target for `primary`/`set_primary`), `PostgresCluster`.
- Produces: `CrdSpecStore::new(client, ns) -> CrdSpecStore` implementing `SpecStore`; `Config.spec_backend: String`.

- [ ] **Step 1:** Config field:

```rust
    /// Where cluster specs live: "configmap" (standalone, default) or "crd"
    /// (operator mode: PostgresCluster resources are the truth).
    #[serde(default = "default_spec_backend")]
    pub spec_backend: String,
```

with `fn default_spec_backend() -> String { "configmap".into() }`, added to `Config::test_default()` too.

- [ ] **Step 2:** `crd_store.rs` (unit tests for the pure part, effective-name injection):

```rust
//! SpecStore over PostgresCluster CRs. The CR is the truth; the primary
//! record stays on the mirror ConfigMap (engine state), so failover CAS is
//! byte-identical in both modes.

use async_trait::async_trait;
use kube::api::{Api, Patch, PatchParams};
use pglord_core::crd::PostgresCluster;
use pglord_core::render::OwnerRef;
use pglord_core::spec::ClusterSpec;

use crate::spec_store::{ConfigMapSpecStore, FIELD_MANAGER, SpecStore};

pub struct CrdSpecStore {
    api: Api<PostgresCluster>,
    /// Delegation target for the primary record on the mirror ConfigMap.
    mirror: ConfigMapSpecStore,
}

impl CrdSpecStore {
    pub fn new(client: kube::Client, ns: &str) -> CrdSpecStore {
        CrdSpecStore {
            api: Api::namespaced(client.clone(), ns),
            mirror: ConfigMapSpecStore::new(client, ns),
        }
    }
}

/// The spec as the engine sees it: metadata.name wins when spec.name is empty.
pub fn effective_spec(cr: &PostgresCluster) -> ClusterSpec {
    let mut spec = cr.spec.clone();
    if spec.name.is_empty()
        && let Some(name) = &cr.metadata.name
    {
        spec.name.clone_from(name);
    }
    spec
}

pub fn cr_owner_ref(cr: &PostgresCluster) -> OwnerRef {
    OwnerRef {
        api_version: "pglord.dev/v1alpha1".into(),
        kind: "PostgresCluster".into(),
        name: cr.metadata.name.clone().unwrap_or_default(),
        uid: cr.metadata.uid.clone().unwrap_or_default(),
    }
}

#[async_trait]
impl SpecStore for CrdSpecStore {
    async fn list(&self) -> anyhow::Result<Vec<ClusterSpec>> {
        let crs = self.api.list(&Default::default()).await?;
        Ok(crs.items.iter().map(effective_spec).collect())
    }

    async fn get(&self, name: &str) -> anyhow::Result<Option<(ClusterSpec, OwnerRef)>> {
        Ok(self
            .api
            .get_opt(name)
            .await?
            .map(|cr| (effective_spec(&cr), cr_owner_ref(&cr))))
    }

    async fn put(&self, spec: &ClusterSpec) -> anyhow::Result<()> {
        let cr = PostgresCluster::new(&spec.name, spec.clone());
        self.api
            .patch(
                &spec.name,
                &PatchParams::apply(FIELD_MANAGER).force(),
                &Patch::Apply(&cr),
            )
            .await?;
        Ok(())
    }

    async fn delete(&self, name: &str) -> anyhow::Result<bool> {
        match self.api.delete(name, &Default::default()).await {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn primary(&self, name: &str) -> anyhow::Result<Option<String>> {
        self.mirror.primary(name).await
    }

    async fn set_primary(&self, name: &str, target: &str) -> anyhow::Result<bool> {
        self.mirror.set_primary(name, target).await
    }
}
```

(Check `spec_store.rs` for whether the trait uses `async_trait` or native async-in-trait — mirror whatever `ConfigMapSpecStore` does. Make `FIELD_MANAGER` and `ConfigMapSpecStore::new` `pub` if not already.)

- [ ] **Step 3:** `main.rs` store selection:

```rust
    let store: std::sync::Arc<dyn spec_store::SpecStore> = match cfg.spec_backend.as_str() {
        "crd" => std::sync::Arc::new(crd_store::CrdSpecStore::new(client.clone(), &ns)),
        _ => std::sync::Arc::new(ConfigMapSpecStore::new(client.clone(), &ns)),
    };
```

and pass `store` into `ClusterOps` (adjust the current inline `Arc::new(...)` at `main.rs:79`).

- [ ] **Step 4:** Unit test for `effective_spec` name injection (pure, no kube) in `crd_store.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_name_fills_an_empty_spec_name() {
        let spec: ClusterSpec =
            serde_json::from_value(serde_json::json!({ "name": "" })).expect("spec");
        let mut cr = PostgresCluster::new("demo", spec);
        assert_eq!(effective_spec(&cr).name, "demo");
        cr.spec.name = "other".into();
        assert_eq!(effective_spec(&cr).name, "other"); // mismatch is the sync's job to flag
    }
}
```

- [ ] **Step 5:** `cargo test -p pglord-server` → PASS; `make lint`. Commit `feat(server): CrdSpecStore and the spec_backend switch`.

---

### Task 4: CR sync loop with status

**Files:**
- Create: `crates/pglord-server/src/cr_sync.rs`
- Modify: `crates/pglord-server/src/lib.rs`, `crates/pglord-server/src/main.rs` (spawn in crd mode), `crates/pglord-server/src/spec_store.rs` (mirror builder helper)

**Interfaces:**
- Consumes: `spec_to_configmap` (make it reusable with owner+extra label), `effective_spec`, `cr_owner_ref`, `recorded_primary` (`primary_record.rs:12`), `spec_label_selector`.
- Produces: `cr_sync::run(client, ns, metrics)`; pure `phase_for(spec, ready, sync_degraded) -> &'static str` and `SyncDecision` used by tests.

- [ ] **Step 1: Failing tests** (pure decision logic, in `cr_sync.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: serde_json::Value) -> pglord_core::spec::ClusterSpec {
        serde_json::from_value(json).expect("spec")
    }

    #[test]
    fn phase_reflects_spec_and_readiness() {
        let s = spec(serde_json::json!({ "name": "demo", "replicas": 2 }));
        assert_eq!(phase_for(&s, 2, false), "Ready");
        assert_eq!(phase_for(&s, 1, false), "Degraded");
        assert_eq!(phase_for(&s, 0, false), "Progressing");
        assert_eq!(phase_for(&s, 2, true), "Degraded");
        let h = spec(serde_json::json!({ "name": "demo", "replicas": 2, "hibernated": true }));
        assert_eq!(phase_for(&h, 0, false), "Hibernated");
    }

    #[test]
    fn name_mismatch_and_invalid_specs_are_flagged_not_synced() {
        let ok = spec(serde_json::json!({ "name": "demo" }));
        assert!(matches!(decide("demo", &ok), SyncDecision::Sync(_)));
        let renamed = spec(serde_json::json!({ "name": "other" }));
        assert!(matches!(decide("demo", &renamed), SyncDecision::Invalid(_)));
        let bad = spec(serde_json::json!({ "name": "demo", "replicas": 99 }));
        assert!(matches!(decide("demo", &bad), SyncDecision::Invalid(_)));
    }
}
```

- [ ] **Step 2:** Run → FAIL. **Step 3: Implement** `cr_sync.rs`:

```rust
//! Operator-mode front loop: watches PostgresCluster resources and converges
//! each one's mirror spec ConfigMap, then reports status. The mirror carries
//! the same labels standalone specs do, so the reconciler, orchestrator and
//! backup scheduler run identically in both modes. Signal-only watch: events
//! wake the loop, the work is always a full sync (reconciler.rs pattern).

use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::watcher;
use pglord_core::crd::{PostgresCluster, PostgresClusterStatus};
use pglord_core::spec::ClusterSpec;

use crate::crd_store::{cr_owner_ref, effective_spec};
use crate::primary_record::recorded_primary;
use crate::spec_store::{FIELD_MANAGER, spec_cm_name, spec_to_configmap_owned};

const RESYNC_INTERVAL: Duration = Duration::from_secs(60);

pub enum SyncDecision {
    Sync(ClusterSpec),
    Invalid(String),
}

/// Name rule + validation, pure. metadata.name is the CR's identity; a
/// non-empty spec.name must agree with it.
pub fn decide(metadata_name: &str, effective: &ClusterSpec) -> SyncDecision {
    if effective.name != metadata_name {
        return SyncDecision::Invalid(format!(
            "spec.name '{}' does not match metadata.name '{}'",
            effective.name, metadata_name
        ));
    }
    match effective.validate() {
        Ok(()) => SyncDecision::Sync(effective.clone()),
        Err(e) => SyncDecision::Invalid(e.to_string()),
    }
}

pub fn phase_for(spec: &ClusterSpec, ready: i32, sync_degraded: bool) -> &'static str {
    if spec.hibernated {
        "Hibernated"
    } else if ready == 0 {
        "Progressing"
    } else if i32::from(spec.replicas) == ready && !sync_degraded {
        "Ready"
    } else {
        "Degraded"
    }
}
```

Then `run()` + `sync_all()` (follow `reconciler.rs:50-146` shape): watcher over `Api::<PostgresCluster>::namespaced`, debounced signal; `sync_all` lists CRs, and per CR:

```rust
async fn sync_one(client: &kube::Client, ns: &str, cr: &PostgresCluster) -> anyhow::Result<()> {
    let name = cr.metadata.name.clone().unwrap_or_default();
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let effective = effective_spec(cr);
    let status = match decide(&name, &effective) {
        SyncDecision::Invalid(msg) => PostgresClusterStatus {
            phase: "Invalid".into(),
            message: msg,
            ..Default::default()
        },
        SyncDecision::Sync(spec) => {
            // Adoption guard: never clobber a ConfigMap-backed cluster.
            let existing = cms.get_opt(&spec_cm_name(&spec.name)).await?;
            let owned_by_me = existing.as_ref().is_none_or(|cm| {
                cm.metadata.owner_references.as_ref().is_some_and(|refs| {
                    refs.iter().any(|r| r.uid == cr.metadata.uid.clone().unwrap_or_default())
                })
            });
            if !owned_by_me {
                PostgresClusterStatus {
                    phase: "Invalid".into(),
                    message: format!("name conflict: a cluster '{}' already exists", spec.name),
                    ..Default::default()
                }
            } else {
                let mirror = spec_to_configmap_owned(&spec, ns, &cr_owner_ref(cr));
                cms.patch(
                    &spec_cm_name(&spec.name),
                    &PatchParams::apply(FIELD_MANAGER).force(),
                    &Patch::Apply(&mirror),
                )
                .await?;
                observe_status(client, ns, &spec, existing.as_ref()).await
            }
        }
    };
    write_status_if_changed(client, ns, cr, status).await
}
```

`spec_to_configmap_owned` in `spec_store.rs`: the existing `spec_to_configmap` plus `owner_references: Some(vec![...])` (from the `OwnerRef`, `controller: true, block_owner_deletion: true`) and label `pglord.dev/internal: "true"`. **No annotations field** — the record CAS keeps ownership of them under SSA.

`observe_status`: `ready_replicas` from `Api::<StatefulSet>::namespaced(...).get_opt(&spec.name)` status; `primary` via `recorded_primary(cm, &spec.name)` on the just-read mirror (fall back `format!("{}-0", spec.name)` when absent); `hibernated: spec.hibernated`; `phase: phase_for(...)` with `sync_degraded: false` for stage A simplicity — the detail API remains the sync-degraded surface (note this in the module docs).

`write_status_if_changed`: compare with `cr.status`, skip when equal; else `api.patch_status(&name, &PatchParams::apply(FIELD_MANAGER).force(), &Patch::Apply(&serde_json::json!({"apiVersion":"pglord.dev/v1alpha1","kind":"PostgresCluster","status": status})))`.

- [ ] **Step 4:** `main.rs`: in crd mode also `tokio::spawn(cr_sync::run(client.clone(), ns.clone(), metrics.clone()))` next to the reconciler spawn.
- [ ] **Step 5:** `cargo test -p pglord-server` → PASS; `make lint`. Commit `feat(server): PostgresCluster sync loop — mirror ConfigMap and status`.

---

### Task 5: Deploy, chart, preflight

**Files:**
- Modify: `deploy/k8s/rbac.yaml`, `deploy/k8s/deployment.yaml`, `charts/pglord/templates/rbac.yaml`, `charts/pglord/templates/deployment.yaml`, `charts/pglord/values.yaml`, `crates/pglord-server/src/preflight.rs`, `crates/pglord-server/src/app.rs` (pass backend to preflight), `crates/pglord-server/src/api/preflight.rs` if `required()` is called there

**Interfaces:**
- Consumes: `Config.spec_backend`.
- Produces: `preflight::required(spec_backend: &str)` (signature change; both call sites updated).

- [ ] **Step 1:** `preflight.rs::required` gains the parameter and appends, when `spec_backend == "crd"`:

```rust
    if spec_backend == "crd" {
        for verb in ["get", "list", "watch", "create", "patch", "delete"] {
            out.push(PermissionCheck { group: "pglord.dev", resource: "postgresclusters", verb });
        }
        for verb in ["get", "patch"] {
            out.push(PermissionCheck { group: "pglord.dev", resource: "postgresclusters/status", verb });
        }
    }
```

Update its unit tests (grep `required()` in preflight.rs tests) to pass `"configmap"` and add one asserting the crd rows appear only in crd mode.

- [ ] **Step 2:** `deploy/k8s/rbac.yaml`: append a commented block (raw manifests are single-mode; the comment says "uncomment for PGLORD_SPEC_BACKEND=crd") with the two rules. `deploy/k8s/deployment.yaml`: add `PGLORD_SPEC_BACKEND` env defaulting to `configmap` (value from a comment-documented edit).
- [ ] **Step 3:** Chart: `values.yaml` gains `specBackend: configmap`; `templates/deployment.yaml` sets the env from it; `templates/rbac.yaml` appends the two rules inside `{{- if eq .Values.specBackend "crd" }}`. `helm template charts/pglord --set specBackend=crd | grep -A3 pglord.dev` sanity-checks.
- [ ] **Step 4:** `cargo test -p pglord-server && make lint` → PASS. Commit `feat(deploy): crd backend RBAC, env and preflight coverage`.

---

### Task 6: Operator e2e + user guide

**Files:**
- Create: `scripts/operator-e2e.sh`
- Modify: `Makefile`, `docs/user-guide.md`

- [ ] **Step 1:** Script, copying the `hibernate-e2e.sh` harness (image build + stale-image verification + port-forward on a fresh port, e.g. 18097). Flow:
  1. `kubectl apply -f deploy/crds/`; patch the pglord Role with the two `pglord.dev` rules (kubectl apply of a small inline manifest); set `PGLORD_SPEC_BACKEND=crd` on the deployment (`kubectl set env`), rollout.
  2. `kubectl apply` an inline PostgresCluster (`metadata.name: crdemo`, `spec: { replicas: 2 }` — no spec.name, proving injection); wait `ready_replicas == 2` via API **and** `kubectl get pgc crdemo -o jsonpath='{.status.phase}'` == `Ready`, printer columns populated.
  3. Write a marker row; `kubectl patch pgc crdemo --type=merge -p '{"spec":{"replicas":3}}'` → API detail converges to 3.
  4. REST edit: `PATCH /clusters/crdemo {"parameters":{"max_connections":"150"}}` → visible in `kubectl get pgc -o yaml`.
  5. Failover check: delete the primary pod → record moves (mirror CAS path) → status.primary updates.
  6. `POST /clusters/crdemo/hibernate` → `kubectl` phase `Hibernated`, 0 pods, PVCs kept → wake → row intact.
  7. Coexistence: create ConfigMap-backed? (standalone backend is off in this deployment — instead assert the adoption guard: `kubectl create configmap other-spec ...` labeled like a spec with no owner, apply CR `other` → its status goes `Invalid` name conflict.)
  8. `kubectl delete pgc crdemo` → children + mirror + PVCs garbage-collected.
  9. Cleanup: reset deployment env to configmap backend, delete leftover objects.
- [ ] **Step 2:** Makefile target `operator-e2e` + help line, run `make operator-e2e` on the stand.
- [ ] **Step 3:** User guide: "Operator mode (CRD)" section — install CRD, RBAC additions, backend flag, kubectl examples (`kubectl get pgc`, patch replicas), name rule, coexistence/adoption guard, status phases table.
- [ ] **Step 4:** Commit `test(e2e): operator mode flow` and `docs(user-guide): operator mode`.

---

## Self-review notes

- Spec coverage: mode flag → T3; CRD/schema/name-default → T1; manifest+drift → T2; sync loop/status/adoption/annotations rule → T4; RBAC/preflight/chart → T5; e2e+docs → T6. Status `sync_degraded` simplification is documented in T4 (deliberate deviation: phase never reports sync-degraded in stage A — actually keep it: `phase_for` accepts the flag and T4 passes `false`; revisit in stage B).
- Type consistency: `effective_spec`/`cr_owner_ref` (T3) consumed by T4; `spec_to_configmap_owned`, `phase_for`, `decide` defined and tested in T4; `required(spec_backend)` change contained in T5.
- kube-derive on `ClusterSpec` generates `PostgresCluster` in `spec` module; `crd.rs` re-exports — one canonical path `pglord_core::crd::PostgresCluster` used everywhere.
