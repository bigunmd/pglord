# Cluster Hibernation and Instance Fencing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Scale a cluster to zero pods with PVCs retained (hibernate/wake) and stop postgres on a chosen standby while its pod stays up (fence/unfence), per `docs/superpowers/specs/2026-08-03-hibernation-fencing-design.md`.

**Architecture:** Two new declarative `ClusterSpec` fields (`hibernated: bool`, `fenced: Vec<String>`) flipped only by dedicated 202 endpoints; the existing level-triggered reconciler and the agent's spec-polling loops converge. Fence entry reuses the stop-and-exit mechanics; a boot-time hold keeps postgres down on fenced pods.

**Tech Stack:** Rust (axum, utoipa, kube-rs), Angular + spartan hlm + Transloco, contract via `make openapi ui-client`.

## Global Constraints

- Never hand-edit `spec/openapi.yaml` or `ui/src/app/api/**`; regenerate with `make openapi ui-client` and commit with the code.
- Clippy denies warnings and `unwrap_used`; Rust edition 2024.
- Problem+json: clients key on the stable `code` field.
- Neither new spec field may alter the rendered pod template (`pod_template_changed` must stay false across hibernate/wake/fence).
- Neither field enters `SpecChange`.
- `deploy/k8s/` and `charts/pglord/` need no changes (no new RBAC: everything works through the existing spec-ConfigMap verbs).

---

### Task 1: Core spec fields + validation

**Files:**
- Modify: `crates/pglord-core/src/spec.rs`

**Interfaces:**
- Produces: `ClusterSpec.hibernated: bool`, `ClusterSpec.fenced: Vec<String>`, `SpecError::InvalidFenced(String)`, `SpecError::DuplicateFenced(String)`.

- [ ] **Step 1: Failing tests** in `spec.rs` `mod tests` (build a valid spec via the existing test helpers/serde pattern):

```rust
#[test]
fn fenced_must_name_an_instance() {
    let spec: ClusterSpec =
        serde_json::from_value(serde_json::json!({ "name": "demo", "replicas": 2, "fenced": ["demo-5"] }))
            .expect("parse");
    assert_eq!(spec.validate(), Err(SpecError::InvalidFenced("demo-5".into())));
    let spec: ClusterSpec =
        serde_json::from_value(serde_json::json!({ "name": "demo", "replicas": 2, "fenced": ["other-1"] }))
            .expect("parse");
    assert_eq!(spec.validate(), Err(SpecError::InvalidFenced("other-1".into())));
}

#[test]
fn fenced_rejects_duplicates_and_accepts_valid_pods() {
    let spec: ClusterSpec =
        serde_json::from_value(serde_json::json!({ "name": "demo", "replicas": 2, "fenced": ["demo-1", "demo-1"] }))
            .expect("parse");
    assert_eq!(spec.validate(), Err(SpecError::DuplicateFenced("demo-1".into())));
    let spec: ClusterSpec =
        serde_json::from_value(serde_json::json!({ "name": "demo", "replicas": 2, "hibernated": true, "fenced": ["demo-1"] }))
            .expect("parse");
    assert_eq!(spec.validate(), Ok(()));
}

#[test]
fn hibernated_and_fenced_default_off() {
    let spec: ClusterSpec = serde_json::from_value(serde_json::json!({ "name": "demo" })).expect("parse");
    assert!(!spec.hibernated);
    assert!(spec.fenced.is_empty());
}
```

- [ ] **Step 2:** `cargo test -p pglord-core fenced` → FAIL (no such fields).
- [ ] **Step 3:** Implement. Fields after `databases`:

```rust
    /// Scale to zero pods, keeping PVCs, credentials and the spec. The
    /// desired replica count is preserved for wake. Flipped only by the
    /// hibernate/wake endpoints, never by PATCH.
    #[serde(default)]
    pub hibernated: bool,
    /// Instances (pod names) held down: the agent stops postgres and keeps
    /// it stopped while listed. Standbys only — the fence endpoint refuses
    /// the recorded primary. Flipped only by the fence/unfence endpoints.
    #[serde(default)]
    pub fenced: Vec<String>,
```

`SpecError` variants:

```rust
    #[error("'{0}' is not an instance of this cluster and cannot be fenced")]
    InvalidFenced(String),
    #[error("instance '{0}' is listed as fenced more than once")]
    DuplicateFenced(String),
```

In `validate()` (exact-name match so `demo-01` never passes for `demo-1`):

```rust
        let valid_pods: Vec<String> = (0..self.replicas)
            .map(|i| format!("{}-{}", self.name, i))
            .collect();
        let mut seen = std::collections::BTreeSet::new();
        for pod in &self.fenced {
            if !valid_pods.contains(pod) {
                return Err(SpecError::InvalidFenced(pod.clone()));
            }
            if !seen.insert(pod.as_str()) {
                return Err(SpecError::DuplicateFenced(pod.clone()));
            }
        }
```

- [ ] **Step 4:** `cargo test -p pglord-core` → PASS (also confirms every existing spec-construction test still compiles; struct literals elsewhere gain the two defaults via serde only — grep for `ClusterSpec {` literals in core/server and add `hibernated: false, fenced: vec![]` or `..Default` as each site requires).
- [ ] **Step 5:** Commit `feat(core): hibernated flag and fenced instance list on the cluster spec`.

---

### Task 2: Core rendering — zero replicas, PVCs retained

**Files:**
- Modify: `crates/pglord-core/src/render.rs` (`:508` STS replicas, `:518-523` retention, `:724` pooler replicas)

**Interfaces:**
- Consumes: `spec.hibernated` from Task 1.

- [ ] **Step 1: Failing tests** in `render.rs` `mod tests`:

```rust
#[test]
fn hibernated_renders_zero_instances_and_pooler_replicas() {
    let spec: crate::spec::ClusterSpec = serde_json::from_value(serde_json::json!({
        "name": "demo", "replicas": 3, "hibernated": true,
        "pooler": { "enabled": true, "replicas": 2 }
    })).expect("parse");
    let sts = render_statefulset(&spec, "ns", &test_owner());
    assert_eq!(sts.spec.as_ref().and_then(|s| s.replicas), Some(0));
    let dep = render_pooler_deployment(&spec, "ns", &test_owner(), "h");
    assert_eq!(dep.spec.as_ref().and_then(|s| s.replicas), Some(0));
}

#[test]
fn pvcs_are_retained_on_scale_down_but_deleted_with_the_cluster() {
    let spec: crate::spec::ClusterSpec =
        serde_json::from_value(serde_json::json!({ "name": "demo" })).expect("parse");
    let policy = render_statefulset(&spec, "ns", &test_owner())
        .spec.and_then(|s| s.persistent_volume_claim_retention_policy).expect("policy");
    assert_eq!(policy.when_scaled.as_deref(), Some("Retain"));
    assert_eq!(policy.when_deleted.as_deref(), Some("Delete"));
}

#[test]
fn hibernation_and_fencing_do_not_touch_the_pod_template() {
    let awake: crate::spec::ClusterSpec =
        serde_json::from_value(serde_json::json!({ "name": "demo", "replicas": 2 })).expect("parse");
    let asleep: crate::spec::ClusterSpec = serde_json::from_value(serde_json::json!({
        "name": "demo", "replicas": 2, "hibernated": true, "fenced": ["demo-1"]
    })).expect("parse");
    let t = |s: &crate::spec::ClusterSpec| render_statefulset(s, "ns", &test_owner()).spec.map(|x| x.template);
    assert_eq!(t(&awake), t(&asleep));
}
```

(Reuse the file's existing owner-fixture helper if one exists; otherwise add `fn test_owner() -> OwnerRef` matching the pattern of existing render tests.)

- [ ] **Step 2:** `cargo test -p pglord-core hibernated_renders` → FAIL.
- [ ] **Step 3:** Implement:
  - `replicas: Some(if spec.hibernated { 0 } else { i32::from(spec.replicas) })` (STS);
  - `when_scaled: Some("Retain".to_string())` with the comment updated: scale-down PVC deletion is the reconciler's explicit `delete_orphan_pvcs`; `Retain` is what makes hibernation non-destructive;
  - pooler deployment `replicas: Some(if spec.hibernated { 0 } else { i32::from(spec.pooler.replicas) })`.
- [ ] **Step 4:** `cargo test -p pglord-core` → PASS.
- [ ] **Step 5:** Commit `feat(core): hibernation renders zero replicas; PVCs retained on scale-down`.

---

### Task 3: Agent fenced mode

**Files:**
- Create: `crates/pglord-agent/src/fenced.rs`
- Modify: `crates/pglord-agent/src/lib.rs` (module list), `src/status.rs`, `src/record.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `ClusterSpec.fenced`, `RecordReader::spec()` (exists).
- Produces: `status::set_fenced(bool)` / `status::is_fenced() -> bool`; `record::is_fenced_in(fenced: &[String], me: &str) -> bool`; `fenced::hold(cfg: Arc<Config>, reader: &RecordReader, me: &str)` (returns when unfenced; caller exits).

- [ ] **Step 1: Failing tests.** In `record.rs` tests:

```rust
#[test]
fn fence_list_matches_exact_pod_name() {
    assert!(is_fenced_in(&["demo-1".into()], "demo-1"));
    assert!(!is_fenced_in(&["demo-1".into()], "demo-0"));
    assert!(!is_fenced_in(&[], "demo-0"));
}
```

In `status.rs` tests (the fenced early-return needs no postgres):

```rust
#[tokio::test]
async fn fenced_flag_overrides_status() {
    set_fenced(true);
    let st = query_status(&Config::test_default()).await;
    assert_eq!(st.role, "fenced");
    assert!(!st.ready);
    set_fenced(false);
}
```

(If `Config::test_default()` does not exist in the agent crate, add the minimal constructor under `#[cfg(test)]` in `config.rs` — defaults for every field, empty cluster name.)

- [ ] **Step 2:** `cargo test -p pglord-agent fenced` → FAIL.
- [ ] **Step 3:** Implement.

`status.rs` — process-wide flag + early return at the top of `query_status`:

```rust
static FENCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn set_fenced(v: bool) { FENCED.store(v, std::sync::atomic::Ordering::Relaxed); }
pub fn is_fenced() -> bool { FENCED.load(std::sync::atomic::Ordering::Relaxed) }
```

```rust
    if is_fenced() {
        return InstanceStatus { role: "fenced", ready: false, in_recovery: None,
            lsn: None, pending_restart: false, sync_standby_count: None, ssl: None };
    }
```

`record.rs` — pure helper + runtime fence entry at the top of the `follow` loop body, before the `!st.ready` early-continue (a crash-looping postgres must not dodge the fence):

```rust
pub fn is_fenced_in(fenced: &[String], me: &str) -> bool {
    fenced.iter().any(|f| f == me)
}
```

```rust
        if let Some(spec) = reader.spec().await
            && is_fenced_in(&spec.fenced, &me)
        {
            tracing::warn!("instance fenced by spec: stopping postgres; restart will hold it down");
            let _ = Command::new("pg_ctl")
                .args(["stop", "-m", "fast", "-D"])
                .arg(&cfg.pgdata)
                .status()
                .await;
            std::process::exit(1);
        }
```

`fenced.rs` — the boot-time hold (serves the control endpoint so `/status` reports `fenced` and `readyz` 503s; polls the spec; returns when unfenced; stays fenced when the spec is unreadable — never start postgres on doubt):

```rust
//! Boot-time hold for fenced instances: postgres is never started; the agent
//! serves its control endpoint (readyz 503, status role "fenced") and waits
//! for the spec to unfence this pod, then returns so main can exit and the
//! pod restarts into a normal boot.

use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::record::{RecordReader, is_fenced_in};
use crate::{http, status};

pub async fn hold(cfg: Arc<Config>, reader: &RecordReader, me: &str) -> anyhow::Result<()> {
    status::set_fenced(true);
    let listener = tokio::net::TcpListener::bind(cfg.http_addr).await?;
    tracing::warn!(addr = %cfg.http_addr, "fenced: postgres held down; control endpoint listening");
    let server = axum::serve(listener, http::router(cfg.clone()));
    tokio::select! {
        result = server.into_future() => { result?; anyhow::bail!("control endpoint stopped while fenced"); }
        () = wait_unfenced(reader, me) => Ok(()),
    }
}

async fn wait_unfenced(reader: &RecordReader, me: &str) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        match reader.spec().await {
            Some(spec) if !is_fenced_in(&spec.fenced, me) => return,
            Some(_) => {}
            None => tracing::debug!("spec unreadable; staying fenced"),
        }
    }
}
```

`main.rs` — after `let cfg = Arc::new(cfg);` and the starting-log line, before the hba/bootstrap block:

```rust
    if let Some(r) = &reader
        && cluster_spec.as_ref().is_some_and(|s| record::is_fenced_in(&s.fenced, &hostname))
    {
        pglord_agent::fenced::hold(cfg.clone(), r, &hostname).await?;
        tracing::warn!("unfenced; exiting so the pod restarts into a normal boot");
        std::process::exit(1);
    }
```

- [ ] **Step 4:** `cargo test -p pglord-agent` → PASS; `cargo clippy --all-targets -- -D warnings`.
- [ ] **Step 5:** Commit `feat(agent): fenced mode — postgres held down while the spec lists the instance`.

---

### Task 4: Server endpoints, guards, DTOs, metrics

**Files:**
- Modify: `crates/pglord-server/src/api/clusters.rs`, `src/api/backups.rs` (create_backup guard), `src/api/secrets.rs` (rotate guard), `src/app.rs` (routes), `src/metrics.rs`
- Test: `crates/pglord-server/tests/http.rs`

**Interfaces:**
- Consumes: spec fields (Task 1).
- Produces: handlers `hibernate`, `wake`, `fence_instance`, `unfence_instance`; helper `reject_hibernated(&ClusterSpec) -> Result<(), ApiError>`; DTO fields `ClusterSummary.hibernated`, `ClusterDetail.hibernated`, `ClusterDetail.fenced`; metric `pglord_cluster_hibernated`.

- [ ] **Step 1: Failing test** — extend `day2_operations_without_kube_answer_503_problem` in `tests/http.rs` with the four routes (asserting the routes exist and answer problem+json):

```rust
        ("POST", "/api/v1/clusters/demo/hibernate", "{}"),
        ("POST", "/api/v1/clusters/demo/wake", "{}"),
        ("POST", "/api/v1/clusters/demo/instances/demo-1/fence", "{}"),
        ("POST", "/api/v1/clusters/demo/instances/demo-1/unfence", "{}"),
```

- [ ] **Step 2:** `cargo test -p pglord-server --test http` → FAIL (404 instead of 503).
- [ ] **Step 3:** Implement in `clusters.rs`:

```rust
/// 409 guard shared by every mutating operation: a hibernated cluster
/// accepts exactly wake, unfence and delete.
pub(crate) fn reject_hibernated(spec: &ClusterSpec) -> Result<(), ApiError> {
    if spec.hibernated {
        return Err(ApiError::conflict("hibernated")
            .with_detail("The cluster is hibernated; wake it first."));
    }
    Ok(())
}
```

Handlers (all `Result<StatusCode, ApiError>`, utoipa-annotated with 202/404/409 — and 422 on fence/unfence — problem+json responses, tag `clusters`; the store `put` triggers the reconciler watch, no manual apply):

```rust
/// Scale the cluster to zero pods, keeping volumes, credentials and spec.
#[utoipa::path(post, path = "/api/v1/clusters/{name}/hibernate", ...)]
pub async fn hibernate(State(state): State<Option<ClusterOps>>, Path(name): Path<String>)
    -> Result<StatusCode, ApiError> {
    let ops = ops(&state)?;
    let Some((mut spec, _owner)) = ops.store.get(&name).await.map_err(ApiError::internal)? else {
        return Err(ApiError::not_found());
    };
    if spec.hibernated {
        return Err(ApiError::conflict("already_hibernated")
            .with_detail("The cluster is already hibernated."));
    }
    spec.hibernated = true;
    ops.store.put(&spec).await.map_err(ApiError::internal)?;
    tracing::warn!(cluster = %name, "HIBERNATE: scaling to zero, PVCs retained");
    Ok(StatusCode::ACCEPTED)
}
```

`wake` mirrors it (`not_hibernated` when already awake, sets `false`, log "WAKE: restoring N instances").

```rust
/// Stop postgres on one standby; the pod stays up for inspection.
#[utoipa::path(post, path = "/api/v1/clusters/{name}/instances/{pod}/fence", ...)]
pub async fn fence_instance(State(state): State<Option<ClusterOps>>,
    Path((name, pod)): Path<(String, String)>) -> Result<StatusCode, ApiError> {
    let ops = ops(&state)?;
    let Some((mut spec, _owner)) = ops.store.get(&name).await.map_err(ApiError::internal)? else {
        return Err(ApiError::not_found());
    };
    reject_hibernated(&spec)?;
    let valid_pods: Vec<String> = (0..spec.replicas).map(|i| format!("{name}-{i}")).collect();
    if !valid_pods.contains(&pod) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_target")
            .with_detail(format!("'{pod}' is not an instance of this cluster.")));
    }
    let primary = ops.store.primary(&name).await.map_err(ApiError::internal)?
        .unwrap_or_else(|| format!("{name}-0"));
    if pod == primary {
        return Err(ApiError::conflict("cannot_fence_primary")
            .with_detail("That instance is the recorded primary; switch over first."));
    }
    if spec.fenced.contains(&pod) {
        return Err(ApiError::conflict("already_fenced").with_detail("Already fenced."));
    }
    spec.fenced.push(pod.clone());
    ops.store.put(&spec).await.map_err(ApiError::internal)?;
    tracing::warn!(cluster = %name, %pod, "FENCE: instance held down by spec");
    Ok(StatusCode::ACCEPTED)
}
```

`unfence_instance`: 404 / `invalid_target` (same pod check) / `not_fenced` when absent; `spec.fenced.retain(|p| p != &pod)`; put; 202. No `reject_hibernated` — unfencing while asleep is allowed by design.

Guards — add `reject_hibernated(&spec)?;` right after the spec fetch in: `patch_cluster`, `switchover`, `restart_cluster` (clusters.rs), `create_backup` (backups.rs), `rotate_credentials` (secrets.rs).

DTOs: add `pub hibernated: bool` to `ClusterSummary` (+ doc "Scaled to zero on purpose; volumes retained") and to `ClusterDetail`; add `pub fenced: Vec<String>` to `ClusterDetail` ("Instances held down by the fence operation"). Fill in `summarize()`/`get_cluster` from the spec (`summarize` sets `hibernated: spec.hibernated`; note `spec.name` is moved last). Instance role override in `get_cluster` (spec.fenced is moved into the DTO, so compute roles first):

```rust
        role: if spec.fenced.contains(&o.pod) { "fenced".to_string() }
              else { role_of(o.in_recovery).to_string() },
```

Update the `InstanceDto.role` doc comment to `"primary" | "standby" | "fenced" | "unknown"`.

`metrics.rs`: add `pub hibernated: IntGaugeVec` = `gauge_vec(&registry, "pglord_cluster_hibernated", "1 while the cluster is hibernated (scaled to zero on purpose).")`; add removal in `forget_cluster`.

`app.rs`: four `.routes(routes!(...))` lines after the switchover/restart ones.

- [ ] **Step 4:** `cargo test -p pglord-server` → PASS; `make lint`.
- [ ] **Step 5:** Commit `feat(server): hibernate/wake and fence/unfence endpoints; hibernated guards on mutating ops`.

---

### Task 5: Control-loop gating (orchestrator, scheduler, reconciler)

**Files:**
- Modify: `crates/pglord-server/src/orchestrator.rs`, `src/backup_scheduler.rs`, `src/reconciler.rs`

**Interfaces:**
- Consumes: spec fields, `metrics.hibernated` (Task 4).

- [ ] **Step 1: Failing test** — `elect_candidate` currently takes the flat observation list; fenced pods are `ready: false` so they are already excluded, but make it explicit and regression-proof. In `orchestrator.rs` tests:

```rust
#[test]
fn fenced_or_unready_pods_are_never_elected() {
    // a fenced pod reports ready=false, in_recovery=None — must not win
    let obs = vec![
        ("demo-1".to_string(), false, None, None),
        ("demo-2".to_string(), true, Some(true), Some("0/5000".to_string())),
    ];
    assert_eq!(elect_candidate(&obs), Some("demo-2".to_string()));
    let only_fenced = vec![("demo-1".to_string(), false, None, None)];
    assert_eq!(elect_candidate(&only_fenced), None);
}
```

(If an equivalent test already exists, extend it with the `None` in_recovery row instead of duplicating.)

- [ ] **Step 2:** `cargo test -p pglord-server elect` → confirm it passes already or fix `elect_candidate`; this pins the contract.
- [ ] **Step 3:** Gating.

`orchestrator.rs` — in the metrics block set the new gauge, then skip hibernated clusters entirely (counter reset stops a stale count firing a failover mid-wake):

```rust
            metrics.hibernated
                .with_label_values(&[spec.name.as_str()])
                .set(i64::from(spec.hibernated));
            if spec.hibernated {
                failures.insert(spec.name.clone(), 0);
                continue;
            }
```

Belt-and-braces on election (a fenced pod could in theory report ready during the stop window): filter it out of the flat list:

```rust
            let flat: Vec<_> = observations
                .iter()
                .filter(|o| !spec.fenced.contains(&o.pod))
                .map(|o| (o.pod.clone(), o.ready, o.in_recovery, o.lsn.clone()))
                .collect();
```

`backup_scheduler.rs` — extend the schedule filter chain:

```rust
                .filter(|_| spec.backup.enabled)
                .filter(|_| !spec.hibernated)
```

`reconciler.rs` — `apply_pooler`: keep applying objects (the Deployment now renders 0 replicas) but skip the agent ensure while hibernated — insert before the `/pooler/ensure` POST:

```rust
    if spec.hibernated {
        return Ok(()); // no primary to ensure auth on; objects above already scaled to zero
    }
```

`apply_provisioning` — first line:

```rust
    if spec.hibernated {
        return Ok(());
    }
```

- [ ] **Step 4:** `cargo test -p pglord-server` → PASS; `make lint`.
- [ ] **Step 5:** Commit `feat(server): control loops skip hibernated clusters and fenced instances`.

---

### Task 6: Contract regeneration

**Files:**
- Regenerate: `spec/openapi.yaml`, `ui/src/app/api/**`

- [ ] **Step 1:** `make openapi ui-client`.
- [ ] **Step 2:** `make openapi-check ui-client-check` → both clean.
- [ ] **Step 3:** Commit `chore(contract): regenerate openapi + ts client for hibernation and fencing`.

---

### Task 7: UI — hibernated state, hibernate/wake, fence/unfence

**Files:**
- Modify: `ui/src/app/pages/clusters/clusters.component.ts`, `cluster-detail.component.ts`, `ui/public/i18n/en.json`

**Interfaces:**
- Consumes: regenerated client (`ClusterSummary.hibernated`, `ClusterDetail.hibernated/fenced`, new service methods — names come out of ng-openapi-gen from the handler operation ids: `hibernate`, `wake`, `fenceInstance`, `unfenceInstance`).

- [ ] **Step 1:** Cluster list: when `c.hibernated`, replace the ready-dots block with a muted "Hibernated" badge (moon glyph, `text-muted-foreground` styling — follow the existing badge idiom in the component).
- [ ] **Step 2:** Cluster detail:
  - banner card when `hibernated` ("Cluster is hibernated — N instances stopped, volumes retained") with the Wake action;
  - Hibernate action next to Restart, opening an hlm confirm dialog (pattern: the existing restart/delete confirms) warning that all instances stop and connections drop;
  - disable Restart / Switchover / Edit / Backup-now / Rotate buttons while hibernated (the server would 409 anyway);
  - per-instance row: a fence action on ready standbys (hidden for the primary) and unfence on fenced rows; role badge renders `fenced` with a distinct muted style (not the error style — fenced is intentional);
  - refresh detail after each action (existing reload pattern).
- [ ] **Step 3:** i18n keys (`clusters.hibernated`, `clusterDetail.hibernate`, `.hibernateConfirm*`, `.wake`, `.hibernatedBanner`, `.fence`, `.unfence`, `.roleFenced`, error toasts for the new problem codes `already_hibernated`, `not_hibernated`, `hibernated`, `cannot_fence_primary`, `already_fenced`, `not_fenced`) in `en.json`.
- [ ] **Step 4:** `make ui-lint ui-build` → clean.
- [ ] **Step 5:** Commit `feat(ui): hibernate/wake and instance fencing with a distinct hibernated state`.

---

### Task 8: E2E + user guide

**Files:**
- Create: `scripts/hibernate-e2e.sh`
- Modify: `Makefile` (target `hibernate-e2e`, listed in help), `docs/user-guide.md`

- [ ] **Step 1:** Script, modeled on `scripts/day2-e2e.sh` (same env/helpers/kubectl context conventions — read it first and copy its harness):
  1. create 2-instance cluster, wait ready, write a marker row via `-rw`;
  2. `POST .../instances/<standby>/fence` → poll until instance role is `fenced` in the detail API and `kubectl exec` confirms no postgres process; cluster still serves reads/writes;
  3. fence the primary → expect 409 `cannot_fence_primary`;
  4. `POST .../unfence` → poll until the standby is ready and streaming again;
  5. `POST .../hibernate` → poll until 0 pods; assert PVCs still exist (`kubectl get pvc`), detail API says `hibernated: true`; `PATCH` and `restart` answer 409 `hibernated`;
  6. `POST .../wake` → poll until all ready; marker row still readable;
  7. delete cluster; assert PVCs are garbage-collected (whenDeleted stays Delete).
- [ ] **Step 2:** Makefile target next to `day2-e2e` (same shape: `bash scripts/hibernate-e2e.sh`).
- [ ] **Step 3:** Run `make hibernate-e2e` against the stand (needs `make stand-up stand-deploy` and a fresh `make stand-image` so the agent image carries Task 3; per the stand memory, verify image IDs before trusting results).
- [ ] **Step 4:** `docs/user-guide.md`: "Hibernation and fencing" section — what each does, PVC retention, the 409 `hibernated` guard list, fence-the-primary refusal and the switchover-first recipe, backup catch-up-after-wake behavior.
- [ ] **Step 5:** Commit `test(e2e): hibernation and fencing flow` and `docs(user-guide): hibernation and fencing`.

---

## Self-review notes

- Spec coverage: every spec section maps to a task (spec surface → 1; rendering → 2; agent → 3; API/guards/DTO/metrics → 4; loop gating → 5; contract → 6; UI → 7; e2e/docs → 8). Wake-with-fenced-instance behavior is emergent from Tasks 2+3 and asserted in e2e step 6 implicitly (fenced list is empty by then; the earlier unfence covered the fenced-boot path via pod restart in step 4).
- Type consistency: `is_fenced_in` used in Tasks 3; `reject_hibernated` only within server task; DTO field names match between Tasks 4 and 7.
- The `hibernated` 409 guard list (patch, switchover, restart, backup, rotate, fence) matches the spec table exactly; unfence deliberately unguarded.
