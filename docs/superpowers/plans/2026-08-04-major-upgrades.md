# Major Version Upgrades (Blue-Green) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Automate PostgreSQL major-version upgrades via a hidden blue-green logical-replication migration on the same cluster identity, plus a published GHCR image matrix (agent 16/17/18 × vanilla/timescaledb, and the server image).

**Architecture:** An upgrade adds a `pglord.dev/upgrade` annotation (CAS'd like the primary record) to the cluster's spec ConfigMap and makes the reconciler render a **second, generation-suffixed StatefulSet** (`<name>-g2`) from the same spec with the target image. The green primary's agent bootstraps in **subscriber mode** (globals via pg_dumpall, schema via pg_dump, per-database publications/subscriptions against the blue `-rw` Service). Cutover fences blue read-only, drains, syncs sequences, drops the pub/sub pairs, then **one CAS** repoints the primary record to the green primary pod — the `-rw` Service selector follows the record, so no separate service swap exists. Finalize persists `generation`/`image` into the spec and deletes the blue StatefulSet (PVCs follow via the existing `whenDeleted: Delete` retention policy).

**Tech Stack:** Rust (axum + utoipa, kube-rs, tokio-postgres), Angular + spartan-ng hlm + Transloco, pgBackRest, GitHub Actions + GHCR, minikube e2e shell scripts.

**Spec:** `docs/superpowers/specs/2026-08-04-major-upgrades-design.md`

## Global Constraints

- Never hand-edit `spec/openapi.yaml`, `ui/src/app/api/**`, or `deploy/crds/*.yaml` — regenerate with `make openapi ui-client crd` and commit together with the code.
- Rust edition 2024; clippy denies warnings and `unwrap_used` — use `expect` only in tests.
- All user-visible UI strings go through Transloco (`ui/public/i18n/en.json`).
- API errors are RFC 9457 problem+json; clients key on the stable `code` field. New codes in this feature: `upgrade_in_progress`, `wal_level_not_logical`, `not_ready_for_cutover`, `no_upgrade_in_progress`, `invalid_upgrade_phase`, `cutover_failed`.
- Verification per task: `cargo test -p <crate>` for the touched crate, `make lint` before each commit.
- `g1` must render byte-identically to today's objects: existing clusters must be untouched by this feature until an upgrade is started.
- Generation label key: `pglord.dev/generation` (only present when generation != "g1" — StatefulSet selectors are immutable, so g1 selectors cannot change).
- Publication name: `pglord_upgrade`. Subscription/slot names: `pglord_upgrade_<sanitized-db>`.
- The agent env var carrying subscriber mode: `PGLORD_AGENT_UPGRADE_SOURCE` (the blue `-rw` service FQDN). Empty/absent = normal instance.

---

### Task 1: Core — `generation` field, instance naming, `UpgradeState`

**Files:**
- Create: `crates/pglord-core/src/upgrade.rs`
- Modify: `crates/pglord-core/src/spec.rs`, `crates/pglord-core/src/lib.rs`
- Regenerate: `deploy/crds/postgresclusters.pglord.dev.yaml` (`make crd`)

**Interfaces:**
- Produces: `ClusterSpec.generation: String` (serde default `"g1"`), `ClusterSpec::sts_name(&self) -> String`, `ClusterSpec::instance_name(&self, ordinal: u8) -> String`, `SpecError::InvalidGeneration(String)`.
- Produces: `pglord_core::upgrade::{UpgradeState, UpgradePhase, UPGRADE_ANNOTATION, next_generation}` re-exported as `pglord_core::upgrade::*` via `pub mod upgrade;` in lib.rs.

- [ ] **Step 1: Write failing tests in `spec.rs`** (append to the existing `mod tests`):

```rust
#[test]
fn generation_defaults_to_g1_and_names_are_unsuffixed() {
    let s = spec("demo");
    assert_eq!(s.generation, "g1");
    assert_eq!(s.sts_name(), "demo");
    assert_eq!(s.instance_name(0), "demo-0");
}

#[test]
fn later_generations_suffix_sts_and_instance_names() {
    let s: ClusterSpec =
        serde_json::from_value(serde_json::json!({ "name": "demo", "generation": "g2" }))
            .expect("deserialize");
    assert_eq!(s.sts_name(), "demo-g2");
    assert_eq!(s.instance_name(1), "demo-g2-1");
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn generation_format_is_validated() {
    for bad in ["", "2", "gen2", "g", "g-1", "G2"] {
        let s: ClusterSpec =
            serde_json::from_value(serde_json::json!({ "name": "demo", "generation": bad }))
                .expect("deserialize");
        assert_eq!(s.validate(), Err(SpecError::InvalidGeneration(bad.into())));
    }
}

#[test]
fn fenced_validates_against_generation_suffixed_names() {
    let s: ClusterSpec = serde_json::from_value(serde_json::json!({
        "name": "demo", "generation": "g2", "replicas": 2, "fenced": ["demo-g2-1"]
    }))
    .expect("deserialize");
    assert_eq!(s.validate(), Ok(()));
    let s: ClusterSpec = serde_json::from_value(serde_json::json!({
        "name": "demo", "generation": "g2", "replicas": 2, "fenced": ["demo-1"]
    }))
    .expect("deserialize");
    assert_eq!(s.validate(), Err(SpecError::InvalidFenced("demo-1".into())));
}
```

- [ ] **Step 2: Run to verify failure**: `cargo test -p pglord-core generation` — expect compile errors (`generation` field missing).

- [ ] **Step 3: Implement in `spec.rs`**. Add the field after `fenced` (order matters only for readability):

```rust
    /// Instance-set generation. "g1" renders today's unsuffixed object names;
    /// a blue-green major upgrade bumps it (g2, g3, …) at finalize, at which
    /// point the StatefulSet and pod names carry the suffix forever (apps are
    /// unaffected — they use the Services). Flipped only by the upgrade
    /// flow, never by PATCH.
    #[serde(default = "default_generation")]
    pub generation: String,
```

with `fn default_generation() -> String { "g1".into() }`, the error variant:

```rust
    #[error("invalid generation '{0}': expected g<number>, e.g. g1")]
    InvalidGeneration(String),
```

the helpers on `impl ClusterSpec`:

```rust
    /// Name of this generation's StatefulSet (and pod-name prefix). g1 keeps
    /// the bare cluster name so pre-generation clusters are untouched.
    pub fn sts_name(&self) -> String {
        if self.generation == "g1" {
            self.name.clone()
        } else {
            format!("{}-{}", self.name, self.generation)
        }
    }

    pub fn instance_name(&self, ordinal: u8) -> String {
        format!("{}-{}", self.sts_name(), ordinal)
    }
```

validation (add before the fenced loop; change the fenced check to use `instance_name`):

```rust
        let valid_generation = self
            .generation
            .strip_prefix('g')
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if !valid_generation {
            return Err(SpecError::InvalidGeneration(self.generation.clone()));
        }
```

and in the fenced loop replace `format!("{}-{}", self.name, i)` with `self.instance_name(i)`.

- [ ] **Step 4: Create `crates/pglord-core/src/upgrade.rs`** with tests in the same file:

```rust
//! Blue-green major upgrade: the durable state stored on the spec ConfigMap
//! and the pure naming/SQL pieces of the logical-replication migration.

use serde::{Deserialize, Serialize};

/// Annotation on the spec ConfigMap holding the serialized UpgradeState.
/// CAS-updated like the primary record; absent = no upgrade in flight.
pub const UPGRADE_ANNOTATION: &str = "pglord.dev/upgrade";

/// One publication of this name per user database on the blue side.
pub const PUBLICATION: &str = "pglord_upgrade";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpgradePhase {
    /// Green exists and is (or is becoming) a logical replica of blue.
    Migrating,
    /// Cutover happened: apps write to green, blue is fenced read-only.
    Soaking,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct UpgradeState {
    pub target_image: String,
    /// The generation the green StatefulSet renders as (current + 1).
    pub next_generation: String,
    pub phase: UpgradePhase,
    /// The recorded blue primary at cutover time; rollback restores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_primary: Option<String>,
}

pub fn next_generation(current: &str) -> String {
    let n: u32 = current
        .strip_prefix('g')
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    format!("g{}", n + 1)
}

/// Subscription (and thus replication-slot) name for a database. Slot names
/// allow only lowercase alphanumerics and underscores; database names allow
/// far more, so everything else maps to '_'.
pub fn subscription_name(db: &str) -> String {
    let safe: String = db
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("pglord_upgrade_{safe}")
}

pub fn create_publication_sql() -> String {
    format!("CREATE PUBLICATION {PUBLICATION} FOR ALL TABLES")
}

pub fn drop_publication_sql() -> String {
    format!("DROP PUBLICATION IF EXISTS {PUBLICATION}")
}

/// `conninfo` is embedded as a SQL string literal; single quotes are doubled.
pub fn create_subscription_sql(db: &str, conninfo: &str) -> String {
    format!(
        "CREATE SUBSCRIPTION \"{}\" CONNECTION '{}' PUBLICATION {PUBLICATION}",
        subscription_name(db),
        conninfo.replace('\'', "''"),
    )
}

pub fn drop_subscription_sql(db: &str) -> String {
    format!("DROP SUBSCRIPTION IF EXISTS \"{}\"", subscription_name(db))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_generation_increments() {
        assert_eq!(next_generation("g1"), "g2");
        assert_eq!(next_generation("g9"), "g10");
        // Garbage never panics; it restarts the count.
        assert_eq!(next_generation("weird"), "g2");
    }

    #[test]
    fn subscription_names_are_slot_safe() {
        assert_eq!(subscription_name("appdb"), "pglord_upgrade_appdb");
        assert_eq!(subscription_name("My-DB.1"), "pglord_upgrade_my_db_1");
    }

    #[test]
    fn subscription_conninfo_quotes_are_escaped() {
        let sql = create_subscription_sql("appdb", "host=x password=it's");
        assert!(sql.contains("password=it''s"));
        assert!(sql.contains("PUBLICATION pglord_upgrade"));
    }

    #[test]
    fn state_roundtrips_through_json() {
        let st = UpgradeState {
            target_image: "pglord-agent:18".into(),
            next_generation: "g2".into(),
            phase: UpgradePhase::Migrating,
            previous_primary: None,
        };
        let json = serde_json::to_string(&st).expect("serialize");
        assert!(json.contains("\"migrating\""));
        let back: UpgradeState = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, st);
    }
}
```

Register in `lib.rs`: `pub mod upgrade;` next to the other modules.

- [ ] **Step 5: Run**: `cargo test -p pglord-core` — expect PASS (all new + existing).
- [ ] **Step 6: Regenerate the CRD**: `make crd && make crd-check` should show the `generation` property in `deploy/crds/postgresclusters.pglord.dev.yaml`.
- [ ] **Step 7: `make lint` and commit**:

```bash
git add crates/pglord-core deploy/crds
git commit -m "feat(core): cluster generation field and blue-green upgrade state types"
```

---

### Task 2: Core — generation-aware render + green StatefulSet

**Files:**
- Modify: `crates/pglord-core/src/render.rs`

**Interfaces:**
- Consumes: `ClusterSpec::{sts_name, instance_name, generation}` (Task 1), `upgrade::UpgradeState`.
- Produces: `render_statefulset` renders name `spec.sts_name()`, selector/pod labels gaining `pglord.dev/generation: <gen>` when generation != "g1", `PGLORD_AGENT_PRIMARY_HOST` pointing at `spec.instance_name(0)`. New `pub fn render_green_statefulset(spec, ns, owner, upgrade: &crate::upgrade::UpgradeState) -> StatefulSet` and `pub fn rw_service_fqdn(cluster: &str, ns: &str) -> String`.

- [ ] **Step 1: Write failing tests** (append to `render.rs` `mod tests`):

```rust
#[test]
fn g1_renders_byte_identically_to_pre_generation_objects() {
    let (spec, owner) = demo();
    let sts = render_statefulset(&spec, "pglord", &owner);
    assert_eq!(sts.metadata.name.as_deref(), Some("demo"));
    let selector = sts.spec.as_ref().expect("spec").selector.clone();
    assert_eq!(selector.match_labels, Some(labels("demo")));
    let pod_labels = sts
        .spec.as_ref().expect("spec")
        .template.metadata.as_ref().expect("meta")
        .labels.clone().expect("labels");
    assert!(!pod_labels.contains_key("pglord.dev/generation"));
}

#[test]
fn g2_suffixes_the_statefulset_and_adds_the_generation_label() {
    let (_, owner) = demo();
    let spec: ClusterSpec = serde_json::from_value(
        serde_json::json!({ "name": "demo", "replicas": 2, "generation": "g2" }),
    )
    .expect("spec");
    let sts = render_statefulset(&spec, "pglord", &owner);
    assert_eq!(sts.metadata.name.as_deref(), Some("demo-g2"));
    let s = sts.spec.expect("spec");
    // Distinct selector: two generations' StatefulSets must never fight
    // over pods, and selectors are immutable so only g2+ carries the label.
    let sel = s.selector.match_labels.expect("labels");
    assert_eq!(sel.get("pglord.dev/generation").map(String::as_str), Some("g2"));
    // Pods still join the one headless service (stable DNS for both sets).
    assert_eq!(s.service_name.as_deref(), Some("demo-pods"));
    // Standbys must basebackup from their own generation's pod 0.
    let env = s.template.spec.expect("pod").containers[0].env.clone().expect("env");
    let primary_host = env.iter().find(|e| e.name == "PGLORD_AGENT_PRIMARY_HOST").expect("var");
    assert_eq!(
        primary_host.value.as_deref(),
        Some("demo-g2-0.demo-pods.pglord.svc.cluster.local")
    );
}

#[test]
fn green_statefulset_carries_target_image_and_upgrade_source() {
    let (spec, owner) = demo();
    let upgrade = crate::upgrade::UpgradeState {
        target_image: "pglord-agent:18".into(),
        next_generation: "g2".into(),
        phase: crate::upgrade::UpgradePhase::Migrating,
        previous_primary: None,
    };
    let sts = render_green_statefulset(&spec, "pglord", &owner, &upgrade);
    assert_eq!(sts.metadata.name.as_deref(), Some("demo-g2"));
    let container = sts.spec.expect("spec").template.spec.expect("pod").containers[0].clone();
    assert_eq!(container.image.as_deref(), Some("pglord-agent:18"));
    let env = container.env.expect("env");
    let source = env.iter().find(|e| e.name == "PGLORD_AGENT_UPGRADE_SOURCE").expect("var");
    assert_eq!(source.value.as_deref(), Some("demo-rw.pglord.svc.cluster.local"));
}
```

- [ ] **Step 2: Run**: `cargo test -p pglord-core render` — expect FAIL (`render_green_statefulset` undefined; g2 assertions fail).

- [ ] **Step 3: Implement.** In `render.rs`:
  1. New label helper below `labels()`:
     ```rust
     /// Cluster labels plus the generation marker. g1 stays label-free:
     /// StatefulSet selectors are immutable, so the original generation's
     /// selector can never change.
     fn instance_labels(spec: &ClusterSpec) -> BTreeMap<String, String> {
         let mut l = labels(&spec.name);
         if spec.generation != "g1" {
             l.insert("pglord.dev/generation".to_string(), spec.generation.clone());
         }
         l
     }
     ```
  2. `pod_labels`: `merged.extend(instance_labels(spec));` instead of `labels(&spec.name)`.
  3. In `render_statefulset`: STS name/meta → `let sts_name = spec.sts_name();` and `meta(&sts_name, ns, &spec.name, owner)`; selector `match_labels: Some(instance_labels(spec))`; `PGLORD_AGENT_PRIMARY_HOST` value → `pod_fqdn(&spec.instance_name(0), &spec.name, ns)`.
  4. New public helpers at the bottom of the non-test code:
     ```rust
     /// DNS name of the cluster's read-write Service inside the cluster.
     pub fn rw_service_fqdn(cluster: &str, ns: &str) -> String {
         format!("{cluster}-rw.{ns}.svc.cluster.local")
     }

     /// The green (next-generation) StatefulSet of an in-flight upgrade: the
     /// same spec rendered one generation up with the target image, plus the
     /// env var that puts its agent into subscriber mode against blue's rw
     /// Service.
     pub fn render_green_statefulset(
         spec: &ClusterSpec,
         ns: &str,
         owner: &OwnerRef,
         upgrade: &crate::upgrade::UpgradeState,
     ) -> StatefulSet {
         let mut derived = spec.clone();
         derived.generation = upgrade.next_generation.clone();
         derived.image = upgrade.target_image.clone();
         // Green bootstraps by subscription, never from a backup repo, and a
         // fenced list naming blue pods has no meaning for the green set.
         derived.restore = None;
         derived.fenced = Vec::new();
         let mut sts = render_statefulset(&derived, ns, owner);
         if let Some(env) = sts
             .spec
             .as_mut()
             .and_then(|s| s.template.spec.as_mut())
             .and_then(|p| p.containers.first_mut())
             .and_then(|c| c.env.as_mut())
         {
             env.push(EnvVar {
                 name: "PGLORD_AGENT_UPGRADE_SOURCE".to_string(),
                 value: Some(rw_service_fqdn(&spec.name, ns)),
                 ..Default::default()
             });
         }
         sts
     }
     ```

- [ ] **Step 4: Run**: `cargo test -p pglord-core` — expect PASS (including all pre-existing render tests: they all use g1 specs and must not change).
- [ ] **Step 5: `make lint` and commit**: `git add crates/pglord-core && git commit -m "feat(core): generation-aware render and green statefulset"`

---

### Task 3: Server — generation-aware naming sweep

**Files:**
- Modify: `crates/pglord-server/src/instance_probe.rs`, `crates/pglord-server/src/primary_record.rs`, `crates/pglord-server/src/spec_store.rs`, `crates/pglord-server/src/reconciler.rs`, `crates/pglord-server/src/orchestrator.rs`, `crates/pglord-server/src/api/clusters.rs`, `crates/pglord-server/src/api/backups.rs`, `crates/pglord-server/src/api/secrets.rs` (and any other `format!("{name}-0")`/`format!("{}-{}", name, i)` site `grep` finds)

**Interfaces:**
- Consumes: `ClusterSpec::{sts_name, instance_name}`.
- Produces: `observe_cluster(http: &reqwest::Client, spec: &ClusterSpec, ns: &str) -> Vec<InstanceObservation>` (spec replaces `cluster` + `replicas` args); `recorded_primary(cm: &ConfigMap, default_pod: &str) -> String` (second arg is now the *pod name* default, callers pass `spec.instance_name(0)`); `roll_order(sts: &str, replicas: u8, primary: &str)` (first arg is the sts prefix).

This task changes no behavior for g1 clusters — every changed expression must reduce to the old string when generation is g1. Work through it mechanically:

- [ ] **Step 1: Find every site**: `grep -rn '\-0"\|{name}-0\|{cluster}-0\|format!("{}-{}"' crates/pglord-server/src/` and `grep -rn 'spec.name' crates/pglord-server/src/reconciler.rs`.
- [ ] **Step 2: Update signatures and call sites** (compiler-driven; the failing build is the test):
  - `instance_probe.rs`: `observe_cluster(http, spec: &ClusterSpec, ns)`; pod names via `spec.instance_name(i)`; FQDN stays `pod_fqdn(&pod, &spec.name, ns)` (the one headless service serves both generations). Update its unit tests to build a `ClusterSpec`.
  - `primary_record.rs`: `recorded_primary(cm, default_pod: &str)` returns the annotation or `default_pod.to_string()`. Update the unit tests (`recorded_primary(&cm, "demo-0")`).
  - `spec_store.rs` `ConfigMapSpecStore::primary`: compute the default from the stored spec —
    ```rust
    Ok(self.api(ns).get_opt(&spec_cm_name(name)).await?.map(|cm| {
        let default_pod = configmap_to_spec(&cm)
            .map(|(s, _)| s.instance_name(0))
            .unwrap_or_else(|_| format!("{name}-0"));
        cm.metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(pglord_core::PRIMARY_ANNOTATION))
            .cloned()
            .unwrap_or(default_pod)
    }))
    ```
  - `reconciler.rs`: `sync_all` passes `recorded_primary(cm, &spec.instance_name(0))`; `apply_cluster` uses `spec.sts_name()` for `sts_api.get_opt/delete/patch` names; `expand_pvcs`/`delete_orphan_pvcs` keep their `cluster: &str` first arg but every **caller** passes `spec.sts_name()` (rename the parameter to `sts` for honesty). `pvc_name` is already `(sts, ordinal)`-shaped — rename its parameter too.
  - `orchestrator.rs`: `observe_cluster(&http, &spec, &item_ns)`; `recorded_primary(cm, &spec.instance_name(0))`.
  - `api/clusters.rs`: every `format!("{name}-0")` fallback → `spec.instance_name(0)`; `valid_pods` → `(0..spec.replicas).map(|i| spec.instance_name(i)).collect()`; `roll_order(&spec.sts_name(), spec.replicas, &primary)` at all three call sites (restart, patch, rotation in `secrets.rs`); `pod_template_changed` needs no change (it compares rendered templates). `expand_pvcs`/`delete_orphan_pvcs` calls in `patch_cluster` pass `&next.sts_name()`.
  - `api/backups.rs`, `api/secrets.rs`: primary-default fallbacks → `spec.instance_name(0)`.
- [ ] **Step 3: Add one regression test** in `orchestrator.rs` tests or `clusters.rs` tests exercising a g2 spec:

```rust
#[test]
fn roll_order_uses_the_generation_prefix() {
    // g2 cluster: standbys ascending, primary last, all suffixed names.
    let order = roll_order("demo-g2", 3, "demo-g2-1");
    assert_eq!(order, vec!["demo-g2-0", "demo-g2-2", "demo-g2-1"]);
}
```

(Adapt to `roll_order`'s actual assertion style at `clusters.rs` if tests for it exist there already.)

- [ ] **Step 4: Run**: `cargo test --workspace` — expect PASS.
- [ ] **Step 5: `make lint` and commit**: `git add crates && git commit -m "refactor(server): generation-aware instance naming throughout"`

---

### Task 4: Agent — foreign-generation record guard

**Files:**
- Modify: `crates/pglord-agent/src/record.rs`, `crates/pglord-agent/src/main.rs`

**Interfaces:**
- Produces: `pub fn same_instance_set(a: &str, b: &str) -> bool` in `record.rs`.
- Behavior: a record naming a pod from another StatefulSet (different prefix before the trailing ordinal) never drives this pod's role — `apply_record_to_config` returns `None` (ordinal fallback decides) and the follow loop takes no promote/fence action. Parameters/fencing/config application still runs every tick.

Why: during an upgrade the one `pglord.dev/primary` record temporarily names a pod of the *other* generation (blue pre-cutover from green's view; green post-cutover from blue's view). Without this guard, a green primary would self-fence the moment it boots, and a fenced blue primary would try to pg_rewind onto a different-major timeline after cutover.

- [ ] **Step 1: Write failing tests** (in `record.rs` `mod tests`):

```rust
#[test]
fn same_instance_set_compares_the_prefix_before_the_ordinal() {
    assert!(same_instance_set("demo-0", "demo-1"));
    assert!(same_instance_set("demo-g2-0", "demo-g2-1"));
    assert!(!same_instance_set("demo-0", "demo-g2-0"));
    assert!(!same_instance_set("demo-g2-0", "demo-g3-0"));
    // Not pod-shaped at all: never claim a match.
    assert!(!same_instance_set("demo", "demo-0"));
    assert!(!same_instance_set("", "demo-0"));
}
```

- [ ] **Step 2: Run**: `cargo test -p pglord-agent same_instance_set` — FAIL (undefined).
- [ ] **Step 3: Implement** in `record.rs` next to `role_from_record`:

```rust
/// Whether two pod names belong to the same StatefulSet (identical prefix
/// before the trailing ordinal). A primary record naming another
/// generation's pod must never drive this pod's role: during a blue-green
/// upgrade the record temporarily points across generations, and acting on
/// it would self-fence the green primary or pg_rewind blue onto a
/// different-major timeline.
pub fn same_instance_set(a: &str, b: &str) -> bool {
    fn prefix(s: &str) -> Option<&str> {
        let (p, ordinal) = s.rsplit_once('-')?;
        (!ordinal.is_empty() && ordinal.chars().all(|c| c.is_ascii_digit())).then_some(p)
    }
    matches!((prefix(a), prefix(b)), (Some(x), Some(y)) if x == y)
}
```

Wire it in:
  - `apply_record_to_config` (record.rs:96-108): first statement after fetching the recorded name — `if !same_instance_set(&recorded, me) { return None; }`.
  - `follow` loop (record.rs:138-161): wrap the promote/fence `match` in `if same_instance_set(&recorded, &me) { ... }` — the earlier steps of the tick (fenced-list check, `apply_parameters`) stay unconditional so green pods keep converging parameters/preload before cutover.
- [ ] **Step 4: Run**: `cargo test -p pglord-agent` — PASS.
- [ ] **Step 5: `make lint` and commit**: `git add crates/pglord-agent && git commit -m "feat(agent): ignore primary records naming another generation"`

---

### Task 5: Agent + probe — `wal_level = logical` everywhere

**Files:**
- Modify: `crates/pglord-agent/src/bootstrap.rs`, `crates/pglord-agent/src/main.rs`, `crates/pglord-agent/src/status.rs`, `crates/pglord-server/src/instance_probe.rs`

**Interfaces:**
- Produces: `bootstrap::ensure_wal_level_logical(pgdata: &Path) -> std::io::Result<bool>`; `InstanceStatus.wal_level: Option<String>`; `InstanceObservation.wal_level: Option<String>`.

- [ ] **Step 1: Failing tests.** In `bootstrap.rs` tests:

```rust
#[test]
fn conf_extra_sets_wal_level_logical() {
    let extra = postgresql_conf_extra(5432);
    assert!(extra.contains("wal_level = logical"));
    assert!(!extra.contains("wal_level = replica"));
}

#[test]
fn ensure_wal_level_logical_rewrites_old_managed_blocks() {
    let dir = tempfile::tempdir().expect("tmp");
    let conf = dir.path().join("postgresql.conf");
    std::fs::write(&conf, "listen_addresses = '*'\nwal_level = replica\n").expect("write");
    assert!(ensure_wal_level_logical(dir.path()).expect("rewrite"));
    let after = std::fs::read_to_string(&conf).expect("read");
    assert!(after.contains("wal_level = logical"));
    // Idempotent: second call is a no-op.
    assert!(!ensure_wal_level_logical(dir.path()).expect("noop"));
}
```

(Existing tests `conf_extra_contains_replication_settings` at bootstrap.rs:419-460 assert `wal_level = replica` — update them to `logical` in the same commit.)

- [ ] **Step 2: Run**: `cargo test -p pglord-agent wal_level` — FAIL.
- [ ] **Step 3: Implement**:
  - `postgresql_conf_extra` (bootstrap.rs:25-45): change the line to `wal_level = logical`. Doc-comment why: logical replication (major upgrades) needs it on every potential publisher; the WAL overhead is modest (CloudNativePG defaults to it).
  - New function in `bootstrap.rs`:
    ```rust
    /// One-time migration of PGDATA created by older agents, which wrote
    /// `wal_level = replica` into postgresql.conf at initdb time. wal_level
    /// only takes effect at postgres start, so rewriting here — before the
    /// supervisor spawns postgres — converges every existing cluster on its
    /// next (rolling) restart.
    pub fn ensure_wal_level_logical(pgdata: &Path) -> std::io::Result<bool> {
        let path = pgdata.join("postgresql.conf");
        let conf = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if !conf.contains("wal_level = replica") {
            return Ok(false);
        }
        std::fs::write(&path, conf.replace("wal_level = replica", "wal_level = logical"))?;
        Ok(true)
    }
    ```
  - `main.rs`: after `bootstrap::ensure_bootstrapped(...)` and before `supervisor::spawn_postgres`, add:
    ```rust
    if bootstrap::ensure_wal_level_logical(&cfg.pgdata)? {
        tracing::info!("rewrote wal_level to logical (takes effect this start)");
    }
    ```
  - `status.rs`: add `pub wal_level: Option<String>` to `InstanceStatus` (default `None` in the error paths); in `try_query`'s SELECT add `current_setting('wal_level')` and map it.
  - `instance_probe.rs`: add `wal_level: Option<String>` to the private `AgentStatus` deserializer and to `InstanceObservation`, defaulting `None` for unreachable pods.
- [ ] **Step 4: Run**: `cargo test --workspace` — PASS (fix any struct-literal sites the compiler flags).
- [ ] **Step 5: `make lint` and commit**: `git add crates && git commit -m "feat(agent): default wal_level=logical, rewrite legacy conf, report via status"`

---

### Task 6: Agent — blue-side endpoints: fence, unfence, cleanup, stanza-upgrade

**Files:**
- Create: `crates/pglord-agent/src/upgrade.rs` (started here, extended in Tasks 7-8)
- Modify: `crates/pglord-agent/src/http.rs`, `crates/pglord-agent/src/backup.rs`, `crates/pglord-agent/src/main.rs` (module registration in `lib.rs` or `main.rs` per crate layout — follow how `postboot` is registered)

**Interfaces:**
- Produces HTTP: `POST /upgrade/fence`, `POST /upgrade/unfence`, `POST /upgrade/cleanup`, `POST /backup/stanza-upgrade` — all 409 unless `in_recovery == Some(false)` (the existing primary-only gate pattern from `take_backup`).
- Produces Rust: `upgrade::fence_writes(cfg) -> anyhow::Result<()>`, `upgrade::unfence_writes(cfg)`, `upgrade::cleanup_source_artifacts(cfg)`, `backup::ensure_stanza_upgrade(cfg)`.

- [ ] **Step 1: Failing test for the pure part.** The SQL fragments live in `pglord-core` already (Task 1); the agent side is thin I/O, so the unit surface here is `backup.rs`'s command construction — follow the existing test style in `backup.rs` if present; otherwise the compile itself gates. Add to `crates/pglord-agent/src/upgrade.rs`:

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn terminate_sql_spares_our_own_backend() {
        assert!(super::TERMINATE_WRITERS_SQL.contains("pid <> pg_backend_pid()"));
        assert!(super::TERMINATE_WRITERS_SQL.contains("client backend"));
    }
}
```

- [ ] **Step 2: Implement `upgrade.rs` (blue half)**:

```rust
//! Blue-green upgrade support. Blue side: write fencing and source-artifact
//! cleanup. Green side (subscriber bootstrap, status, cutover) lives here
//! too — see run_subscriber below (Tasks 7-8).

use crate::config::Config;

pub(crate) const TERMINATE_WRITERS_SQL: &str = "SELECT pg_terminate_backend(pid) \
     FROM pg_stat_activity \
     WHERE pid <> pg_backend_pid() AND backend_type = 'client backend'";

/// Make this (blue) primary read-only for the cutover window. Persisted via
/// ALTER SYSTEM so a pod restart during soak stays fenced; rollback resets.
pub async fn fence_writes(cfg: &Config) -> anyhow::Result<()> {
    let (client, _conn) = crate::postboot::connect_local(cfg).await?;
    client
        .batch_execute("ALTER SYSTEM SET default_transaction_read_only = on")
        .await?;
    client.batch_execute("SELECT pg_reload_conf()").await?;
    // Open write transactions would otherwise commit after the drain check.
    client.batch_execute(TERMINATE_WRITERS_SQL).await?;
    Ok(())
}

pub async fn unfence_writes(cfg: &Config) -> anyhow::Result<()> {
    let (client, _conn) = crate::postboot::connect_local(cfg).await?;
    client
        .batch_execute("ALTER SYSTEM RESET default_transaction_read_only")
        .await?;
    client.batch_execute("SELECT pg_reload_conf()").await?;
    Ok(())
}

/// Drop upgrade publications and any leftover inactive replication slots on
/// this (blue) primary. Idempotent; called on abort and rollback so a dead
/// green side can never leak slots that pin WAL forever.
pub async fn cleanup_source_artifacts(cfg: &Config) -> anyhow::Result<()> {
    let (client, _conn) = crate::postboot::connect_local(cfg).await?;
    let dbs = client
        .query(
            "SELECT datname FROM pg_database \
             WHERE datallowconn AND NOT datistemplate",
            &[],
        )
        .await?;
    for row in dbs {
        let db: String = row.get(0);
        let (c, _h) = crate::postboot::connect_local_db(cfg, &db).await?;
        c.batch_execute(&pglord_core::upgrade::drop_publication_sql())
            .await?;
    }
    client
        .execute(
            "SELECT pg_drop_replication_slot(slot_name) FROM pg_replication_slots \
             WHERE slot_name LIKE 'pglord\\_upgrade\\_%' AND NOT active",
            &[],
        )
        .await?;
    Ok(())
}
```

`connect_local` / `connect_local_db` in `postboot.rs` are private today — make them `pub(crate)`.

- [ ] **Step 3: `backup.rs`** — mirror `ensure_stanza` (backup.rs:60):

```rust
/// After a major upgrade the stanza still describes the old PG version;
/// stanza-upgrade re-points it at the current data directory.
pub async fn ensure_stanza_upgrade(cfg: &Config) -> anyhow::Result<()> {
    let out = command(cfg, &["stanza-upgrade"]).output().await?;
    if !out.status.success() {
        anyhow::bail!("{}", pgbackrest_error(&out.stdout, &out.stderr, out.status));
    }
    Ok(())
}
```

- [ ] **Step 4: `http.rs`** — add routes and handlers following the `take_backup` gate shape exactly (409 JSON `{"error": ...}` when `st.in_recovery != Some(false)`):

```rust
.route("/upgrade/fence", post(upgrade_fence))
.route("/upgrade/unfence", post(upgrade_unfence))
.route("/upgrade/cleanup", post(upgrade_cleanup))
.route("/backup/stanza-upgrade", post(stanza_upgrade))
```

Each handler: primary gate, then call the module function, map `Err` to `(StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": err.to_string()})))`, `Ok` to `Json(json!({"status": "ok"}))`.

- [ ] **Step 5: Run**: `cargo test -p pglord-agent` — PASS. `make lint`.
- [ ] **Step 6: Commit**: `git add crates/pglord-agent && git commit -m "feat(agent): blue-side upgrade endpoints (fence, cleanup, stanza-upgrade)"`

---

### Task 7: Agent — subscriber-mode bootstrap (green side)

**Files:**
- Modify: `crates/pglord-agent/src/upgrade.rs`, `crates/pglord-agent/src/config.rs`, `crates/pglord-agent/src/main.rs`, `crates/pglord-agent/src/record.rs` (archive-off guard)

**Interfaces:**
- Consumes: `PGLORD_AGENT_UPGRADE_SOURCE` env (blue rw FQDN) → `Config.upgrade_source: String` (default "").
- Produces: `upgrade::run_subscriber(cfg: Arc<Config>)` spawned from `main` on the green primary; PGDATA markers `pglord.upgrade-globals`, `pglord.upgrade-schema-<db>`, `pglord.upgrade-complete`; `upgrade::source_conninfo(cfg, db) -> String`.
- Invariant: while `upgrade_source` is set, `apply_parameters` renders archiving **disabled** (green must not archive-push into blue's stanza — wrong system id until stanza-upgrade at finalize; the template loses the env var at finalize's roll, which re-enables archiving).

- [ ] **Step 1: Failing tests** (in `upgrade.rs`):

```rust
#[test]
fn source_conninfo_targets_the_blue_rw_service() {
    let mut cfg = crate::config::test_config(); // reuse/introduce the test helper other agent tests use
    cfg.upgrade_source = "demo-rw.pglord.svc.cluster.local".into();
    cfg.superuser = "postgres".into();
    cfg.superuser_password = "pw".into();
    let ci = source_conninfo(&cfg, "appdb");
    assert!(ci.contains("host=demo-rw.pglord.svc.cluster.local"));
    assert!(ci.contains("dbname=appdb"));
    assert!(ci.contains("user=postgres"));
}

#[test]
fn subscriber_markers_gate_each_step() {
    let dir = tempfile::tempdir().expect("tmp");
    assert!(!marker_exists(dir.path(), "globals"));
    write_marker(dir.path(), "globals").expect("write");
    assert!(marker_exists(dir.path(), "globals"));
    // db names are sanitized the same way subscription names are
    write_marker(dir.path(), &schema_marker("My-DB")).expect("write");
    assert!(marker_exists(dir.path(), &schema_marker("My-DB")));
}
```

(If no `test_config()` helper exists in `config.rs`, add one behind `#[cfg(test)]` building a `Config` with defaults — check how `bootstrap.rs`/`postboot.rs` tests construct `Config` and follow that instead.)

- [ ] **Step 2: Implement.**
  - `config.rs`: add field
    ```rust
    /// Blue-green upgrades: when set, this instance belongs to the green
    /// (next-generation) set and its primary bootstraps as a logical
    /// subscriber of the named host (the blue rw Service). Empty = normal.
    #[serde(default)]
    pub upgrade_source: String,
    ```
  - `upgrade.rs` green half:
    ```rust
    pub fn source_conninfo(cfg: &Config, db: &str) -> String {
        format!(
            "host={} port={} user={} password={} dbname={}",
            cfg.upgrade_source, cfg.primary_port, cfg.superuser, cfg.superuser_password, db
        )
    }

    fn marker_path(pgdata: &Path, step: &str) -> PathBuf {
        pgdata.join(format!("pglord.upgrade-{step}"))
    }
    pub(crate) fn marker_exists(pgdata: &Path, step: &str) -> bool { marker_path(pgdata, step).exists() }
    pub(crate) fn write_marker(pgdata: &Path, step: &str) -> std::io::Result<()> {
        std::fs::write(marker_path(pgdata, step), b"")
    }
    pub(crate) fn schema_marker(db: &str) -> String {
        format!("schema-{}", pglord_core::upgrade::subscription_name(db))
    }
    ```
  - `run_subscriber(cfg: Arc<Config>)`: loop with 10 s backoff until `try_establish` returns Ok, then return; skip everything immediately if `marker_exists(&cfg.pgdata, "complete")` (a green pod restarting during soak must not re-subscribe to a fenced blue). Log each attempt's error at warn.
  - `try_establish(cfg) -> anyhow::Result<()>` — each step idempotent:
    0. **Direction guard.** Connect to blue (`connect_source(cfg, "postgres")`), `SELECT current_setting('server_version_num')::int`, compare with green's own `SHOW server_version_num` on the local socket: `bail!` when green's major is **below** blue's ("green runs PostgreSQL {g}, source runs {b}; a major upgrade cannot go backwards"). The server cannot judge an arbitrary image tag, so this is the only place the direction is truly known; the error surfaces through `/upgrade/status`'s `error` field and the detail block.
    1. Wait until `status::query_status(cfg).await.ready`.
    2. **Globals** (skip when `marker_exists(pgdata, "globals")`): run
       `pg_dumpall --globals-only -h <upgrade_source> -p <primary_port> -U <superuser>` with `PGPASSWORD` env, capture stdout to a temp file under `cfg.pgdata.parent()`; apply with `psql -U <superuser> -h <socket_dir> -p <port> -d postgres --no-psqlrc -f <file>` **without** ON_ERROR_STOP (pre-existing roles error harmlessly on retry). Write the marker on success.
    3. **Enumerate databases** on blue (`connect_source(cfg, "postgres")` — a `tokio_postgres::connect` with `source_conninfo`, `NoTls`): `SELECT d.datname, pg_get_userbyid(d.datdba) FROM pg_database d WHERE d.datallowconn AND NOT d.datistemplate AND d.datname <> 'postgres'`.
    4. Per database, on green local: `SELECT 1 FROM pg_database WHERE datname = $1` else `CREATE DATABASE "<db>" OWNER "<owner>"` (identifiers double-quoted with `"` doubled; owner comes from blue so it exists after step 2).
    5. **Schema** per db (skip when `marker_exists(pgdata, &schema_marker(db))`): `pg_dump -h <source> -p <port> -U <superuser> --schema-only --no-publications --no-subscriptions -f <tmpfile> <db>` then `psql ... -d <db> -v ON_ERROR_STOP=1 --no-psqlrc -f <tmpfile>`; marker on success.
    6. **Publication** per db on blue (`connect_source(cfg, db)`): `SELECT 1 FROM pg_publication WHERE pubname = 'pglord_upgrade'` else `create_publication_sql()`.
    7. **Subscription** per db on green local db: `SELECT 1 FROM pg_subscription WHERE subname = $1` (param `subscription_name(db)`) else `create_subscription_sql(db, &source_conninfo(cfg, db))`.
  - `main.rs`: after the postboot spawns (main.rs:110-123), add:
    ```rust
    if !cfg_arc.upgrade_source.is_empty() && cfg_arc.role == Role::Primary {
        let cfg_up = cfg_arc.clone();
        tokio::spawn(async move { pglord_agent::upgrade::run_subscriber(cfg_up).await });
    }
    ```
    (match how `postboot` tasks are actually spawned there — same Arc/clone pattern.)
  - `record.rs` `apply_parameters`: where the archive conf is rendered from the spec's backup block, force it off while in subscriber/green mode:
    ```rust
    let archive_enabled = spec.backup.enabled && cfg.upgrade_source.is_empty();
    ```
    with a comment: green archiving into blue's stanza would fail (system-id mismatch) and retain WAL; finalize's roll drops the env var and re-enables it after stanza-upgrade.
- [ ] **Step 3: Run**: `cargo test -p pglord-agent` — PASS. `make lint`.
- [ ] **Step 4: Commit**: `git add crates/pglord-agent && git commit -m "feat(agent): subscriber-mode green bootstrap via logical replication"`

---

### Task 8: Agent — `/upgrade/status` and `/upgrade/cutover` (green side)

**Files:**
- Modify: `crates/pglord-agent/src/upgrade.rs`, `crates/pglord-agent/src/http.rs`

**Interfaces:**
- Produces HTTP on the green primary:
  - `GET /upgrade/status` → 200 `UpgradeAgentStatus` JSON:
    ```rust
    #[derive(Debug, Serialize, Deserialize)]
    pub struct UpgradeAgentStatus {
        pub established: bool,          // all subscriptions exist
        pub complete: bool,             // cutover already ran (marker)
        pub databases: Vec<DbSync>,     // one per user database
        pub lag_bytes: Option<i64>,     // max over upgrade slots on blue
        pub error: Option<String>,
    }
    #[derive(Debug, Serialize, Deserialize)]
    pub struct DbSync { pub name: String, pub tables_total: i64, pub tables_ready: i64 }
    ```
    (`Deserialize` too — the server reuses this type via a local mirror struct in Task 10; keep field names stable.)
  - `POST /upgrade/cutover` → drains, syncs sequences, drops subs+pubs, writes the `complete` marker; 200 `{"status":"ok"}` / 409 when not established / 500 with the error.

- [ ] **Step 1: Failing test** (pure drain-target comparison, in `upgrade.rs`):

```rust
#[test]
fn drained_requires_every_slot_at_or_past_the_target() {
    // (confirmed_flush_lsn - target) >= 0 for every upgrade slot.
    assert!(drained(&[0, 5]));
    assert!(!drained(&[-1, 5]));
    assert!(drained(&[]) == false); // no slots at all: nothing established, refuse
}
```

- [ ] **Step 2: Implement**:
  - `fn drained(diffs: &[i64]) -> bool { !diffs.is_empty() && diffs.iter().all(|d| *d >= 0) }`
  - `pub async fn status(cfg: &Config) -> UpgradeAgentStatus`:
    - green local dbs (same enumeration query, local): for each with a subscription, `SELECT COUNT(*) FILTER (WHERE srsubstate = 'r'), COUNT(*) FROM pg_subscription_rel r JOIN pg_subscription s ON s.oid = r.srsubid WHERE s.subname = $1` → `DbSync`.
    - lag from blue (`connect_source(cfg, "postgres")`): `SELECT COALESCE(MAX(pg_wal_lsn_diff(pg_current_wal_lsn(), confirmed_flush_lsn))::bigint, 0) FROM pg_replication_slots WHERE slot_name LIKE 'pglord\\_upgrade\\_%'` — `None` on connection error, with the error string in `error`.
    - `established` = every enumerated blue user db has a subscription on green; `complete` = the marker.
  - `pub async fn cutover(cfg: &Config) -> anyhow::Result<()>`:
    1. Refuse (`bail!`) unless `status(cfg).await.established`.
    2. Blue `postgres` connection: `SELECT pg_current_wal_lsn()::text` → target.
    3. Poll ≤ 30 × 2 s: `SELECT pg_wal_lsn_diff(confirmed_flush_lsn, $1::pg_lsn)::bigint FROM pg_replication_slots WHERE slot_name LIKE 'pglord\\_upgrade\\_%'` → `drained(&diffs)`; timeout → `bail!("drain timed out; blue still has unreplicated WAL")`.
    4. Per db: blue db conn `SELECT schemaname, sequencename, COALESCE(last_value, 0) FROM pg_sequences`; green local db conn `SELECT setval(format('%I.%I', $1::text, $2::text)::regclass, $3 + 1000, true)` per row — the +1000 margin absorbs sequence values cached by blue backends that never hit WAL.
    5. Per green db: `drop_subscription_sql(db)` (drops the remote slot while blue is reachable — that is why detach precedes any blue teardown).
    6. Per blue db: `drop_publication_sql()`.
    7. `write_marker(&cfg.pgdata, "complete")`.
  - `http.rs`: `GET /upgrade/status` — 404 JSON `{"error":"not an upgrade instance"}` when `cfg.upgrade_source.is_empty()`, else 200 with `upgrade::status(&cfg).await`. `POST /upgrade/cutover` — same 404 guard, then map `cutover` result (200 / 500). No primary gate on status (standbys just report empty); cutover keeps the primary gate.
- [ ] **Step 3: Run**: `cargo test -p pglord-agent` — PASS. `make lint`.
- [ ] **Step 4: Commit**: `git add crates/pglord-agent && git commit -m "feat(agent): green upgrade status and cutover execution"`

---

### Task 9: Server — upgrade-state store (CAS annotations)

**Files:**
- Modify: `crates/pglord-server/src/primary_record.rs`, `crates/pglord-server/src/spec_store.rs`, `crates/pglord-server/src/crd_store.rs`

**Interfaces:**
- Produces: `primary_record::cas_set_annotations(api, cm_name, expected_rv, annotations: serde_json::Map<String, serde_json::Value>) -> anyhow::Result<bool>` (null value deletes a key; `cas_set_primary` becomes a one-key wrapper).
- Produces on `SpecStore`:
  ```rust
  async fn upgrade_state(&self, ns: &str, name: &str) -> anyhow::Result<Option<UpgradeState>>;
  /// CAS-write the upgrade annotation (None clears it), optionally flipping
  /// the primary record in the SAME patch — cutover and rollback need the
  /// two to change atomically. False = lost the race; re-read and retry.
  async fn set_upgrade(&self, ns: &str, name: &str, state: Option<&UpgradeState>, set_primary_to: Option<&str>) -> anyhow::Result<bool>;
  ```
- Produces: `spec_store::upgrade_from_cm(cm: &ConfigMap) -> Option<UpgradeState>` (pure; unparseable annotation → None with a warn).
- `CrdSpecStore` delegates both to its mirror `ConfigMapSpecStore` exactly the way `primary`/`set_primary` already do.

- [ ] **Step 1: Failing tests** (in `spec_store.rs` tests — pure part only):

```rust
#[test]
fn upgrade_state_parses_from_the_annotation() {
    use pglord_core::upgrade::{UPGRADE_ANNOTATION, UpgradePhase};
    let mut cm = ConfigMap::default();
    assert!(upgrade_from_cm(&cm).is_none());
    cm.metadata.annotations = Some(
        [(UPGRADE_ANNOTATION.to_string(),
          r#"{"target_image":"a:18","next_generation":"g2","phase":"migrating"}"#.to_string())]
        .into(),
    );
    let st = upgrade_from_cm(&cm).expect("state");
    assert_eq!(st.phase, UpgradePhase::Migrating);
    assert_eq!(st.next_generation, "g2");
    // Garbage must not take the reconciler down.
    cm.metadata.annotations = Some([(UPGRADE_ANNOTATION.to_string(), "{".to_string())].into());
    assert!(upgrade_from_cm(&cm).is_none());
}
```

- [ ] **Step 2: Run** — FAIL. **Step 3: Implement**:
  - `primary_record.rs`: generalize —
    ```rust
    /// Compare-and-swap a set of annotations on the spec ConfigMap in one
    /// merge patch (a null value removes the key). Cutover flips the primary
    /// record and the upgrade phase in the same patch so no observer can see
    /// one without the other.
    pub async fn cas_set_annotations(
        api: &Api<ConfigMap>,
        cm_name: &str,
        expected_rv: &str,
        annotations: serde_json::Map<String, serde_json::Value>,
    ) -> anyhow::Result<bool> {
        let patch = serde_json::json!({
            "metadata": { "resourceVersion": expected_rv, "annotations": annotations }
        });
        match api.patch(cm_name, &PatchParams::default(), &Patch::Merge(&patch)).await {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    ```
    Reimplement `cas_set_primary` as a wrapper building the one-key map.
  - `spec_store.rs`: `upgrade_from_cm` (parse + `tracing::warn!` on error), trait methods, `ConfigMapSpecStore` impl: `upgrade_state` = get CM → `upgrade_from_cm`; `set_upgrade` = get CM (None → Ok(false)), build the annotation map (`UPGRADE_ANNOTATION` → serialized state or `serde_json::Value::Null`; plus `PRIMARY_ANNOTATION` when `set_primary_to` is Some), `cas_set_annotations` with the read rv.
  - `crd_store.rs`: forward both to the inner ConfigMap store the same way `primary`/`set_primary` are forwarded.
- [ ] **Step 4: Run**: `cargo test -p pglord-server` — PASS. `make lint`.
- [ ] **Step 5: Commit**: `git add crates/pglord-server && git commit -m "feat(server): upgrade-state storage with atomic annotation CAS"`

---

### Task 10: Server — reconciler renders green; orchestrator + scheduler guards

**Files:**
- Modify: `crates/pglord-server/src/reconciler.rs`, `crates/pglord-server/src/orchestrator.rs`, `crates/pglord-server/src/backup_scheduler.rs`

**Interfaces:**
- Produces: `reconciler::apply_cluster(client, http, ns, spec, owner, primary_pod, upgrade: Option<&UpgradeState>)` — new last parameter. When `Some`, after the main STS apply it also applies `render_green_statefulset(spec, ns, owner, upgrade)` with the same field manager.
- Every existing `apply_cluster` caller updated: `sync_all` and `orchestrator` pass `upgrade_from_cm(cm).as_ref()`; `patch_cluster` and `run_rotation` pass `None` (they are 409-blocked during upgrades in Task 11, so no green cluster can exist on their path).
- Orchestrator: a cluster whose upgrade phase is `Soaking` is skipped entirely (failure counter reset, no election). `Migrating` keeps normal blue failover.
- Backup scheduler: a cluster whose upgrade phase is `Soaking` is skipped with an info log (blue is fenced, green's stanza is pre-upgrade).

- [ ] **Step 1: Failing test** (orchestrator guard is the riskiest line — test the pure decision):

Add to `orchestrator.rs`:

```rust
/// Pure: should the orchestrator manage this cluster this tick?
pub fn failover_enabled(upgrade: Option<&pglord_core::upgrade::UpgradeState>) -> bool {
    !matches!(upgrade.map(|u| u.phase), Some(pglord_core::upgrade::UpgradePhase::Soaking))
}
```

with test:

```rust
#[test]
fn no_failover_during_soak() {
    use pglord_core::upgrade::{UpgradePhase, UpgradeState};
    let st = |phase| UpgradeState {
        target_image: "a:18".into(), next_generation: "g2".into(),
        phase, previous_primary: None,
    };
    assert!(failover_enabled(None));
    assert!(failover_enabled(Some(&st(UpgradePhase::Migrating))));
    // During soak the record names a green pod the blue observation set
    // cannot contain; electing a blue standby would hijack writes back
    // onto the old major. Manual gate territory.
    assert!(!failover_enabled(Some(&st(UpgradePhase::Soaking))));
}
```

- [ ] **Step 2: Run** — FAIL. **Step 3: Implement**:
  - `reconciler.rs`: add the parameter; after `sts_api.patch(...)` for the main STS:
    ```rust
    if let Some(u) = upgrade {
        let green = pglord_core::render::render_green_statefulset(spec, ns, owner, u);
        let green_name = green.metadata.name.clone().unwrap_or_default();
        sts_api.patch(&green_name, &pp, &Patch::Apply(&green)).await?;
    }
    ```
    `sync_all`: `let upgrade = crate::spec_store::upgrade_from_cm(cm);` → pass `upgrade.as_ref()`.
  - `orchestrator.rs`: after parsing the cm, before probing:
    ```rust
    let upgrade = crate::spec_store::upgrade_from_cm(cm);
    if !failover_enabled(upgrade.as_ref()) {
        failures.insert((item_ns.clone(), spec.name.clone()), 0);
        continue;
    }
    ```
    and its post-failover `apply_cluster` call passes `upgrade.as_ref()`.
  - `backup_scheduler.rs`: where a due cluster is about to fire, fetch `store.upgrade_state(ns, name)` (or parse from the cm if the scheduler already holds it) and skip `Soaking` with `tracing::info!(cluster, "scheduled backup skipped: upgrade soaking")`.
- [ ] **Step 4: Run**: `cargo test --workspace` — PASS. `make lint`.
- [ ] **Step 5: Commit**: `git add crates/pglord-server && git commit -m "feat(server): render green statefulset; guard failover and backups during soak"`

---

### Task 11: Server — upgrade API (start/cutover/rollback/abort/finalize) + day-2 guards + detail block

**Files:**
- Create: `crates/pglord-server/src/api/upgrade.rs`
- Modify: `crates/pglord-server/src/api/mod.rs`, `crates/pglord-server/src/api/clusters.rs`, `crates/pglord-server/src/api/secrets.rs`, `crates/pglord-server/src/app.rs`
- Regenerate: `spec/openapi.yaml`, `ui/src/app/api/**` (`make openapi ui-client`)

**Interfaces:**
- Produces routes (all under `/api/v1/namespaces/{ns}/clusters/{name}`): `POST /upgrade` (body `UpgradeRequest { target_image: String }`, 202/404/409/422), `POST /upgrade/cutover`, `POST /upgrade/rollback`, `POST /upgrade/abort`, `POST /upgrade/finalize` (each 202/404/409).
- Produces DTOs (`#[derive(Serialize, ToSchema)]`): 
  ```rust
  pub struct UpgradeStatusDto {
      pub phase: pglord_core::upgrade::UpgradePhase,
      pub target_image: String,
      pub next_generation: String,
      pub ready_to_cutover: bool,
      pub lag_bytes: Option<i64>,
      pub databases: Vec<UpgradeDbDto>,   // { name, tables_total, tables_ready }
      pub error: Option<String>,
  }
  ```
  and `ClusterDetail` gains `pub upgrade: Option<UpgradeStatusDto>`.
- Produces: `clusters.rs` `pub(crate) async fn reject_upgrading(ops: &ClusterOps, ns: &str, name: &str) -> Result<(), ApiError>` (409 `upgrade_in_progress`), called at the top of `patch_cluster`, `restart_cluster`, `switchover`, `hibernate`, `fence_instance`, `unfence_instance`, and `rotate_credentials` (secrets.rs), and in `backups.rs`' manual `create_backup` **only when phase is Soaking** (manual blue backups during migration stay allowed — the UI suggests one before cutover).
- Green primary pod name: `format!("{}-{}-0", spec.name, state.next_generation)` — factor as `fn green_primary_pod(spec: &ClusterSpec, st: &UpgradeState) -> String`.
- Cutover readiness rule (server-side): `established && !databases.is_empty() == false` is fine — precisely: `established && databases.iter().all(|d| d.tables_total == d.tables_ready) && lag_bytes.is_some_and(|l| l <= 1_048_576)`; a cluster with zero user databases is trivially ready when `established`.

- [ ] **Step 1: Failing tests.** In `api/upgrade.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn agent_status(established: bool, ready: (i64, i64), lag: Option<i64>) -> AgentUpgradeStatus {
        AgentUpgradeStatus {
            established,
            complete: false,
            databases: vec![AgentDbSync { name: "appdb".into(), tables_total: ready.0, tables_ready: ready.1 }],
            lag_bytes: lag,
            error: None,
        }
    }

    #[test]
    fn ready_to_cutover_needs_sync_and_low_lag() {
        assert!(ready_to_cutover(&agent_status(true, (3, 3), Some(0))));
        assert!(ready_to_cutover(&agent_status(true, (3, 3), Some(1_048_576))));
        assert!(!ready_to_cutover(&agent_status(true, (3, 2), Some(0))));
        assert!(!ready_to_cutover(&agent_status(true, (3, 3), Some(2_000_000))));
        assert!(!ready_to_cutover(&agent_status(true, (3, 3), None)));
        assert!(!ready_to_cutover(&agent_status(false, (3, 3), Some(0))));
    }

    #[test]
    fn zero_database_clusters_are_trivially_ready() {
        let st = AgentUpgradeStatus {
            established: true, complete: false, databases: vec![],
            lag_bytes: Some(0), error: None,
        };
        assert!(ready_to_cutover(&st));
    }
}
```

And in `tests/http.rs` (contract presence, mirroring the existing pattern at http.rs:296-308):

```rust
for path in [
    "/api/v1/namespaces/{ns}/clusters/{name}/upgrade",
    "/api/v1/namespaces/{ns}/clusters/{name}/upgrade/cutover",
    "/api/v1/namespaces/{ns}/clusters/{name}/upgrade/rollback",
    "/api/v1/namespaces/{ns}/clusters/{name}/upgrade/abort",
    "/api/v1/namespaces/{ns}/clusters/{name}/upgrade/finalize",
] {
    assert!(api.paths.paths.contains_key(path), "missing {path}");
}
```

- [ ] **Step 2: Run** — FAIL. **Step 3: Implement `api/upgrade.rs`.** Local mirror of the agent JSON — deliberately a separate type from the agent's `UpgradeAgentStatus`/`DbSync` (the crates do not depend on each other; the wire field names are the contract): `#[derive(Deserialize)] struct AgentUpgradeStatus { established: bool, complete: bool, databases: Vec<AgentDbSync>, lag_bytes: Option<i64>, error: Option<String> }`, `#[derive(Deserialize)] struct AgentDbSync { name: String, tables_total: i64, tables_ready: i64 }`, `fn ready_to_cutover(&AgentUpgradeStatus) -> bool` per the rule above, agent URL helper `format!("http://{}:{}/upgrade/{verb}", pod_fqdn(pod, &spec.name, ns), AGENT_HTTP_PORT)`.

Handlers (each follows the `restart_cluster` shape: `ops(&state)?` → `store.get` → 404 → guards → work → `StatusCode::ACCEPTED`):

**`start_upgrade`** (`POST .../upgrade`):
1. `reject_hibernated(&spec)?`; 422 `invalid_image` when `req.target_image.trim().is_empty()`.
2. 409 `upgrade_in_progress` when `ops.store.upgrade_state(&ns, &name).await?` is Some.
3. `let primary = ops.store.primary(...)...`; `observe_cluster(&ops.http, &spec, &ns)`, find the primary observation: `wal_level` must be `Some("logical")` — else 409 `wal_level_not_logical` with detail `"the primary runs wal_level={other}; perform a rolling restart first (Restart cluster), then retry"`; unreachable primary → 409 `wal_level_not_logical` with an "unreachable" detail.
4. Build `UpgradeState { target_image: trimmed, next_generation: next_generation(&spec.generation), phase: Migrating, previous_primary: None }`; `set_upgrade(..., Some(&state), None)` — on `false` return 409 `upgrade_in_progress` (a concurrent writer).
5. Synchronously `reconciler::apply_cluster(..., &primary, Some(&state))` so the green STS exists before the 202.

**`cutover_upgrade`**: state must be Some with `phase == Migrating` (else 409 `no_upgrade_in_progress` / `invalid_upgrade_phase`). GET green `/upgrade/status` (2 s timeout) → 409 `not_ready_for_cutover` unless `ready_to_cutover`. Then:
1. POST blue primary `/upgrade/fence` — on error: best-effort `/upgrade/unfence`, return `ApiError::new(StatusCode::BAD_GATEWAY, "cutover_failed").with_detail(...)`.
2. POST green primary `/upgrade/cutover` with `.timeout(Duration::from_secs(180))` — on error: best-effort unfence blue, same `cutover_failed` (state stays Migrating; retryable).
3. `set_upgrade(..., Some(&UpgradeState { phase: Soaking, previous_primary: Some(primary.clone()), ..state }), Some(&green_primary_pod(&spec, &state)))` — retry once on CAS `false` after re-reading; on persistent failure unfence blue and error.
4. `reconciler::apply_cluster(..., &green_primary_pod(...), Some(&new_state))` to repoint `-rw` immediately.

**`rollback_upgrade`**: phase must be `Soaking`. `let prev = state.previous_primary.clone().unwrap_or_else(|| spec.instance_name(0));`
1. `set_upgrade(..., None, Some(&prev))` (atomic: record back to blue + state cleared).
2. `reconciler::apply_cluster(..., &prev, None)`.
3. Best-effort (warn on error): POST blue `/upgrade/unfence`, POST blue `/upgrade/cleanup`.
4. Delete the green STS: `Api::<StatefulSet>::namespaced(...).delete(&format!("{}-{}", spec.name, state.next_generation), &Default::default())` — its PVCs follow via the `whenDeleted: Delete` retention policy.

**`abort_upgrade`**: phase must be `Migrating`. `set_upgrade(..., None, None)`; delete the green STS (as above); best-effort POST blue `/upgrade/cleanup` (drops pubs and any leaked slots that would pin WAL). 

**`finalize_upgrade`**: phase must be `Soaking`.
1. `let mut next = spec.clone(); next.generation = state.next_generation.clone(); next.image = state.target_image.clone();` `ops.store.put(&ns, &next)`.
2. If `next.backup.enabled`: POST green primary `/backup/stanza-upgrade` — failure is a warn (level-triggered backups will keep erroring visibly until re-run; document).
3. Delete the **blue** STS (`spec.sts_name()`, i.e. the pre-bump name) — PVCs follow via retention policy.
4. `set_upgrade(..., None, None)` to clear the annotation (record already names the green pod).
5. `reconciler::apply_cluster(..., green primary, None)` with the *new* spec, then spawn the existing roll (`roll_order(&next.sts_name(), next.replicas, &green_primary)` + `roll_pods`) — the roll removes `PGLORD_AGENT_UPGRADE_SOURCE` from live pods, which re-enables WAL archiving on green.

**`clusters.rs`**: `reject_upgrading` helper (see Interfaces) + call sites; `ClusterDetail.upgrade` filled in `get_cluster`: when `upgrade_state` is Some — for `Migrating`, GET green `/upgrade/status` and map to the DTO (`unreachable` → `established:false`, error string set); for `Soaking`, a DTO with `ready_to_cutover:false`, empty databases, `error:None`.

**`app.rs`**: five `.routes(routes!(...))` lines (cutover/rollback/abort/finalize each on its own path → separate `routes!` calls; `start_upgrade` alone on `/upgrade`).

- [ ] **Step 4: Run**: `cargo test --workspace` — PASS.
- [ ] **Step 5: Regenerate contract**: `make openapi ui-client` then `make openapi-check ui-client-check`.
- [ ] **Step 6: `make lint` and commit**:

```bash
git add crates spec/openapi.yaml ui/src/app/api
git commit -m "feat(server): blue-green upgrade API with day-2 guards and detail status"
```

---

### Task 12: UI — upgrade card on cluster detail

**Files:**
- Create: `ui/src/app/pages/clusters/cluster-upgrade-dialog.component.ts`
- Modify: `ui/src/app/pages/clusters/cluster-detail.component.ts`, `ui/public/i18n/en.json`

**Interfaces:**
- Consumes generated client fns (names follow the handler fn names): `startUpgrade`, `cutoverUpgrade`, `rollbackUpgrade`, `abortUpgrade`, `finalizeUpgrade` from `ui/src/app/api/fn/clusters/...`, and `ClusterDetail.upgrade`.
- Produces: an "upgrade" section on the detail page and a new top-level Transloco namespace `upgrade`.

- [ ] **Step 1: i18n.** Add to `ui/public/i18n/en.json` (top-level key `upgrade`, alongside `image`):

```json
"upgrade": {
  "title": "Major version upgrade",
  "start": "Start upgrade",
  "startTitle": "Upgrade {{name}} to a new major version?",
  "targetImage": "Target image",
  "startWarning": "A parallel set of instances on the new version is created and kept in sync by logical replication. Applications are unaffected until you cut over. Do not run schema changes (DDL) until the upgrade is finished.",
  "phase": "Phase",
  "migrating": "Replicating to the new version",
  "soaking": "Cut over — old instances on standby",
  "ready": "Ready to cut over",
  "notReady": "Synchronizing…",
  "unreachable": "New instances not reachable yet",
  "lag": "Replication lag",
  "tables": "{{ready}}/{{total}} tables synced",
  "cutover": "Cut over",
  "cutoverTitle": "Cut over {{name}} now?",
  "cutoverWarning": "Writes are briefly rejected while the last changes drain and sequences sync. Take a final backup first if you need one — point-in-time recovery does not cross a major upgrade.",
  "rollback": "Roll back",
  "rollbackTitle": "Roll back {{name}} to the old version?",
  "rollbackWarning": "All writes made on the new version since cutover are abandoned. The old instances resume exactly where they were fenced.",
  "finalize": "Finalize",
  "finalizeTitle": "Finalize the upgrade of {{name}}?",
  "finalizeWarning": "The old instances and their volumes are deleted. This cannot be rolled back.",
  "abort": "Abort",
  "abortTitle": "Abort the upgrade of {{name}}?",
  "abortWarning": "The new instances and their volumes are deleted. The running cluster is unaffected."
}
```

- [ ] **Step 2: Dialogs.** `cluster-upgrade-dialog.component.ts` exports two components following the file's conventions (`ImageDialogComponent` for the form, `RestartDialogComponent` for confirms — see cluster-detail.component.ts:169-302):
  - `UpgradeStartDialogComponent` — context `{ name: string }`, one `HlmInput` bound to `targetImage = signal('')`, `canStart = computed(() => this.targetImage().trim().length > 0)`, warning paragraph `upgrade.startWarning`, `ref.close(this.targetImage().trim())`, confirm button `data-testid="confirm-upgrade-start"`.
  - `UpgradeConfirmDialogComponent` — context `{ name: string; kind: 'cutover' | 'rollback' | 'finalize' | 'abort' }`; title/warning keys derived (`'upgrade.' + kind + 'Title'`); destructive kinds (`rollback`, `finalize`, `abort`) use `variant="destructive"` on the confirm button; `data-testid="confirm-upgrade-" + kind`; `ref.close(true|false)`.
- [ ] **Step 3: Detail section.** In `cluster-detail.component.ts` template, after the config section: a "Major version upgrade" header row. When `d.upgrade` is null: a `variant="outline" size="sm"` button `data-testid="open-upgrade"` (disabled when `busy() || d.hibernated`) opening the start dialog and calling `startUpgrade` via the established `run()` pattern. When `d.upgrade` is set: a bordered status card showing phase (`upgrade.migrating`/`upgrade.soaking`), target image (mono), per-database `upgrade.tables` lines, lag in bytes when present, and the action buttons:
  - Migrating: `Cut over` (`data-testid="upgrade-cutover"`, disabled unless `d.upgrade.ready_to_cutover`) + `Abort` (ghost, `data-testid="upgrade-abort"`).
  - Soaking: `Finalize` (`data-testid="upgrade-finalize"`) + `Roll back` (ghost destructive, `data-testid="upgrade-rollback"`).
  Each button: confirm dialog → `run(() => this.api.invoke(<fn>, { ns: this.ns(), name: this.name() }))`. The 5 s poll (`REFRESH_MS`) already refreshes the block.
- [ ] **Step 4: Verify**: `make ui-lint ui-build` (and `make ui-test` — existing specs must stay green).
- [ ] **Step 5: Commit**: `git add ui && git commit -m "feat(ui): major-version upgrade card and dialogs"`

---

### Task 13: Images — parameterized agent Dockerfile + GHCR publish workflow

**Files:**
- Modify: `deploy/Dockerfile.agent`
- Create: `.github/workflows/images.yaml`

**Interfaces:**
- Produces images: `ghcr.io/<owner>/pglord-agent:{16,17,18}`, `...:{16,17,18}-timescaledb`, `ghcr.io/<owner>/pglord-server:latest` — all also tagged `vX.Y.Z-...` on version tags, built (not pushed) on PRs touching `deploy/**` or the workflow.

- [ ] **Step 1: Dockerfile.agent** — parameterize the runtime stage (builder stages unchanged; one Rust build serves every base):

```dockerfile
ARG PG_MAJOR=17
FROM postgres:${PG_MAJOR} AS runtime
ARG PG_MAJOR
ARG TIMESCALEDB=false
# pgBackRest ships in the pgdg repo the postgres image already trusts.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pgbackrest \
    && rm -rf /var/lib/apt/lists/*
# TimescaleDB variant: the extension must exist in the image for both the
# extensions feature (preload) and major upgrades (target image must ship
# every extension the cluster uses).
RUN if [ "$TIMESCALEDB" = "true" ]; then \
        apt-get update \
        && apt-get install -y --no-install-recommends curl ca-certificates gnupg lsb-release \
        && curl -fsSL https://packagecloud.io/timescale/timescaledb/gpgkey \
           | gpg --dearmor -o /usr/share/keyrings/timescaledb.gpg \
        && echo "deb [signed-by=/usr/share/keyrings/timescaledb.gpg] https://packagecloud.io/timescale/timescaledb/debian/ $(lsb_release -cs) main" \
           > /etc/apt/sources.list.d/timescaledb.list \
        && apt-get update \
        && apt-get install -y --no-install-recommends "timescaledb-2-postgresql-${PG_MAJOR}" \
        && apt-get purge -y curl gnupg lsb-release \
        && rm -rf /var/lib/apt/lists/*; \
    fi
```

(The `ARG PG_MAJOR` line must appear before the first `FROM` **and** be re-declared inside the stage; keep the existing pgbackrest layer, chown lines, agent COPY, USER/EXPOSE/ENTRYPOINT unchanged below.)

- [ ] **Step 2: Verify locally**: `docker build -f deploy/Dockerfile.agent --build-arg PG_MAJOR=16 -t pglord-agent:16-test . && docker run --rm --entrypoint postgres pglord-agent:16-test --version` → prints `postgres (PostgreSQL) 16.x`. Default build (`make agent-image`) must still produce a PG 17 image.
- [ ] **Step 3: Workflow** `.github/workflows/images.yaml`:

```yaml
name: images
on:
  push:
    branches: [main, master]
    tags: ["v*"]
  pull_request:
    paths: ["deploy/Dockerfile*", ".github/workflows/images.yaml"]

env:
  REGISTRY: ghcr.io

jobs:
  agent:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    strategy:
      fail-fast: false
      matrix:
        pg: ["16", "17", "18"]
        timescaledb: [false, true]
    steps:
      - uses: actions/checkout@v4
      - uses: docker/setup-buildx-action@v3
      - uses: docker/login-action@v3
        if: github.event_name != 'pull_request'
        with:
          registry: ${{ env.REGISTRY }}
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - id: suffix
        run: echo "s=${{ matrix.timescaledb == true && '-timescaledb' || '' }}" >> "$GITHUB_OUTPUT"
      - uses: docker/metadata-action@v5
        id: meta
        with:
          images: ${{ env.REGISTRY }}/${{ github.repository_owner }}/pglord-agent
          tags: |
            type=raw,value=${{ matrix.pg }}${{ steps.suffix.outputs.s }},enable={{is_default_branch}}
            type=semver,pattern={{version}}-pg${{ matrix.pg }}${{ steps.suffix.outputs.s }}
      - uses: docker/build-push-action@v6
        with:
          context: .
          file: deploy/Dockerfile.agent
          build-args: |
            PG_MAJOR=${{ matrix.pg }}
            TIMESCALEDB=${{ matrix.timescaledb }}
          push: ${{ github.event_name != 'pull_request' }}
          tags: ${{ steps.meta.outputs.tags }}
          labels: ${{ steps.meta.outputs.labels }}
          cache-from: type=gha,scope=agent
          cache-to: type=gha,scope=agent,mode=max

  server:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4
      - uses: docker/setup-buildx-action@v3
      - uses: docker/login-action@v3
        if: github.event_name != 'pull_request'
        with:
          registry: ${{ env.REGISTRY }}
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - uses: docker/metadata-action@v5
        id: meta
        with:
          images: ${{ env.REGISTRY }}/${{ github.repository_owner }}/pglord-server
          tags: |
            type=raw,value=latest,enable={{is_default_branch}}
            type=semver,pattern={{version}}
      - uses: docker/build-push-action@v6
        with:
          context: .
          file: deploy/Dockerfile
          push: ${{ github.event_name != 'pull_request' }}
          tags: ${{ steps.meta.outputs.tags }}
          labels: ${{ steps.meta.outputs.labels }}
          cache-from: type=gha,scope=server
          cache-to: type=gha,scope=server,mode=max
```

Note: the `matrix.timescaledb == true && '-timescaledb' || ''` expression form is the documented GH Actions ternary; verify with `gh workflow` lint or a PR run. If PG 18 packages are missing from a repo at build time (pgdg/timescale lag new majors), let that matrix cell fail — `fail-fast: false` keeps the rest publishing — and note it in the PR.

- [ ] **Step 4: Commit**: `git add deploy/Dockerfile.agent .github/workflows/images.yaml && git commit -m "ci(images): publish agent (16/17/18 x vanilla/timescaledb) and server images to GHCR"`

---

### Task 14: E2e — `scripts/upgrade-e2e.sh`

**Files:**
- Create: `scripts/upgrade-e2e.sh` (chmod +x)
- Modify: `Makefile` (target `upgrade-e2e` following the `day2-e2e` pattern at Makefile:82-83)

Follow `scripts/day2-e2e.sh` conventions exactly: `set -euo pipefail`, `KUBECTL="minikube -p pglord kubectl --"`, port `18102`, `trap cleanup EXIT`, `kill_pf`, python3 JSON extraction, wait loops `for _ in $(seq 1 N); do ...; sleep 2; done`. Respect the e2e pitfalls memory: `get`-by-name under pipefail, never assume primary identity after rolls, verify image IDs after loads.

- [ ] **Step 1: Write the script.** Skeleton (fill helpers verbatim from day2-e2e.sh:5-42):

```bash
#!/usr/bin/env bash
# Blue-green major upgrade e2e: 16 -> 17 with rollback first, then the real
# cutover + finalize. Needs the dev stand (make stand-up stand-deploy).
set -euo pipefail
KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18102
# ... kill_pf / cleanup / trap / port-forward exactly as day2-e2e.sh ...

# Two real majors: same builder layers, different runtime base.
docker build -f deploy/Dockerfile.agent --build-arg PG_MAJOR=16 -t pglord-agent:dev16 .
docker build -f deploy/Dockerfile.agent --build-arg PG_MAJOR=17 -t pglord-agent:dev17 .
for img in pglord-agent:dev16 pglord-agent:dev17; do
  minikube -p pglord image rm "docker.io/library/$img" 2>/dev/null || true
  minikube -p pglord image load "$img"
  want=$(docker image inspect "$img" -f '{{.Id}}')
  have=$(minikube -p pglord ssh -- docker image inspect -f '{{.Id}}' "docker.io/library/$img" 2>/dev/null | tr -d '\r')
  [ "$want" = "$have" ] || { echo "FAIL: minikube kept a stale $img"; exit 1; }
done

# Cluster on 16 with a database, data, a sequence and an extension.
curl -sf -X POST "$API/api/v1/clusters" -H 'content-type: application/json' -d '{
  "name": "updemo", "image": "pglord-agent:dev16", "replicas": 2,
  "databases": [{"name": "appdb"}],
  "extensions": [{"name": "pgcrypto", "database": "appdb"}]
}' >/dev/null
# wait ready_replicas == 2 (poll $API/api/v1/clusters/updemo)
# seed: psql_on updemo-0 (appdb): CREATE TABLE t(id bigserial primary key, v text); INSERT 100 rows
# assert wal_level: psql_on updemo-0 "SHOW wal_level" == logical

start_upgrade() { curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/updemo/upgrade" \
  -H 'content-type: application/json' -d '{"target_image":"pglord-agent:dev17"}'; }
upgrade_field() { curl -s "$API/api/v1/clusters/updemo" | python3 -c "import sys,json; u=json.load(sys.stdin).get('upgrade') or {}; print(u.get('$1',''))"; }

# --- Scenario A: cutover then ROLLBACK ---
[ "$(start_upgrade)" = "202" ]
# poll upgrade_field ready_to_cutover == True (up to 120 x 2s; green must basebackup + sync)
# day-2 guard: PATCH replicas must 409 with code upgrade_in_progress
# cutover: POST .../upgrade/cutover -> 202; poll phase == soaking
# write a marker row on the NEW primary (via the rw service from inside a pod)
# rollback: POST .../upgrade/rollback -> 202; poll: upgrade block gone
# assert: writes work again on blue (INSERT), marker row from green is absent,
#         green sts updemo-g2 gone, pvcs data-updemo-g2-* gone

# --- Scenario B: cutover then FINALIZE ---
[ "$(start_upgrade)" = "202" ]
# poll ready_to_cutover; INSERT rows right up to cutover; cutover -> soaking
# assert row count survived and sequence continues: INSERT returns id > seeded max
# assert psql_on <green pod> "SHOW server_version" starts with 17
# finalize: POST .../upgrade/finalize -> 202
# poll: upgrade block gone; sts updemo gone; sts updemo-g3 (or -g2 — read
#       next_generation from the API before finalizing, don't hardcode) has 2 ready pods
# assert pgcrypto usable in appdb on the new primary:
#       SELECT encode(digest('x','sha256'),'hex') works
# assert cluster detail: ready_replicas == 2, image == pglord-agent:dev17
echo "PASS: upgrade e2e"
```

Notes for the implementer: pod names of the green set come from the API (`instances` in the detail) — never hardcode `-g2` vs `-g3` (scenario B runs after a rollback, so the generation counter has moved); rollback in scenario A leaves generation at g1, but `next_generation` in scenario B is still `g2` because rollback cleared the state before finalize ever bumped the spec — read it from the API regardless. The write-fence assertion in scenario A: after cutover, `psql_on <blue primary> "INSERT ..."` must fail with `read-only`.

- [ ] **Step 2: Makefile**: `upgrade-e2e: ## Blue-green major upgrade e2e against the dev stand` + `./scripts/upgrade-e2e.sh`.
- [ ] **Step 3: Run it** against the stand: `make stand-up stand-deploy && make upgrade-e2e`. Fix what fails — this task is where the integration truth lives; expect iterations (drain timing, readiness polling). Do not weaken assertions to pass.
- [ ] **Step 4: Commit**: `git add scripts/upgrade-e2e.sh Makefile && git commit -m "test(e2e): blue-green major upgrade (rollback and finalize scenarios)"`

---

### Task 15: Docs

**Files:**
- Modify: `docs/user-guide.md`, `docs/superpowers/specs/2026-08-04-major-upgrades-design.md`, `CLAUDE.md` (repo one — commands list gains `upgrade-e2e`)

- [ ] **Step 1: user-guide.** Add `### Major version upgrades` directly after `### Minor version upgrades` (user-guide.md:235-255): the flow (start → watch sync → cut over → soak → finalize/rollback), the published image matrix (`ghcr.io/<owner>/pglord-agent:17`, `-timescaledb` variants; pick the same variant one or more majors up), the DDL freeze rule, large objects not replicated, sequences handled with a margin, the write outage at cutover, PITR not crossing the upgrade (take a final backup before cutover), WAL archiving on the new instances resuming at finalize, no automatic failover during soak, and the `wal_level_not_logical` remedy (one rolling restart for clusters created before this version). Update `## 1. Install` to reference the GHCR images, fix the stale line at user-guide.md:637 ("Changing the cluster name or image after creation is not supported." → name only), and update `## Current limitations`.
- [ ] **Step 2: Spec doc deltas.** Append a short "Implementation notes" section to the design doc recording the three deviations discovered during implementation: (1) roles/databases are migrated via `pg_dumpall --globals-only` + enumeration from blue (covers objects created outside the spec — logical replication breaks without them), not only from declarative provisioning; (2) there is no separate service-swap step — the `-rw` Service follows the primary record, so cutover is a single record+phase CAS; (3) scheduled-backup suspension during soak is a skip-with-log, and green WAL archiving is disabled until finalize's roll (bounded-soak advice).
- [ ] **Step 3: Commit**: `git add docs CLAUDE.md && git commit -m "docs: major version upgrades user guide and spec implementation notes"`

---

## Execution order & checkpoints

Tasks 1→2→3 are strictly sequential (core types → render → sweep). Task 4-8 (agent) and 9-11 (server) each depend on 1-3; within each group the order given is dependency order. Task 12 needs 11's regenerated client. Task 13 is independent (can run any time). Task 14 needs everything except 12/15. Suggested checkpoints for review: after Task 3 (`cargo test --workspace` green, zero behavior change on g1), after Task 11 (full contract regenerated), after Task 14 (e2e green on the stand).
