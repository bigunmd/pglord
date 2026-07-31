# Workload Customization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make pglord clusters schedulable and survivable in a real cluster: anti-affinity so instances do not share a node, CPU/memory requests and limits, plus the customization hooks operators expect (labels, annotations, StorageClass, imagePullSecrets, nodeSelector, tolerations, priorityClass).

**Architecture:** A `workload` block on `ClusterSpec` rendered into the StatefulSet's pod template and volume claim template. User labels land on the pod template only — never on the StatefulSet selector, which is immutable and load-bearing for the `-rw` service — and managed labels are merged last so a user label can never shadow them.

**Tech Stack:** existing.

## Global Constraints

- Anti-affinity defaults to **preferred** (soft). Required would leave single-node clusters (our own minikube stand, and plenty of dev clusters) permanently Pending; preferred spreads where possible and still schedules where not.
- Label/annotation keys under `app.kubernetes.io/`, `pglord.dev/`, and `statefulset.kubernetes.io/` are **rejected**: those select and identify pglord's own objects.
- `storage_class` is immutable after creation (StatefulSet `volumeClaimTemplates` cannot be patched); reject the change with a clear message instead of letting the apiserver fail the apply.
- Resource quantities are shape-validated (`500m`, `2`, `1Gi`) before they reach Kubernetes.
- API change ⇒ regenerate `spec/openapi.yaml` + `ui/src/app/api` in the same commit.

## Tasks

### W1: Workload spec in core
**Files:** `crates/pglord-core/src/workload.rs` (new), `src/spec.rs`, `src/lib.rs`

```rust
pub enum AntiAffinity { Preferred, Required, Disabled }   // default Preferred
pub struct Resources { cpu_request, cpu_limit, memory_request, memory_limit: Option<String> }
pub struct Toleration { key, operator, value, effect: Option<String> }
pub struct WorkloadSpec {
    labels: BTreeMap<String,String>, annotations: BTreeMap<String,String>,
    resources: Resources, storage_class: Option<String>, image_pull_secrets: Vec<String>,
    node_selector: BTreeMap<String,String>, tolerations: Vec<Toleration>,
    priority_class_name: Option<String>, anti_affinity: AntiAffinity,
}
pub fn validate(w: &WorkloadSpec) -> Result<(), WorkloadError>;
pub fn is_reserved_key(key: &str) -> bool;
pub fn is_quantity(s: &str) -> bool;   // 500m | 2 | 1Gi | 512Mi
```
`ClusterSpec` gains `#[serde(default)] pub workload: WorkloadSpec`; `validate()` calls into it. `SpecChange` gains `workload: Option<WorkloadSpec>`; `apply_change` rejects a `storage_class` that differs from the current one.

Tests: reserved key rejected (each prefix); bad quantity rejected; valid resources accepted; default is `Preferred`; storage_class change rejected, same value accepted.

### W2: Render into the StatefulSet
**Files:** `crates/pglord-core/src/render.rs`

- Pod template labels = user labels merged **then** managed labels (managed win).
- Pod template annotations = user annotations.
- Container `resources` from `Resources` (omit entirely when empty).
- Pod spec: `node_selector`, `tolerations`, `image_pull_secrets`, `priority_class_name`.
- `affinity.pod_anti_affinity` on `kubernetes.io/hostname` matching `app.kubernetes.io/instance=<cluster>`: preferred (weight 100) / required / none.
- PVC template `storage_class_name`.

Tests: managed labels survive a colliding user label; selector untouched by user labels; preferred by default and required when asked; resources present/absent; storage class applied; tolerations and pull secrets land.

### W3: API + UI
- `ClusterDetail` gains `workload`; `PATCH` accepts it (`invalid_workload` 422 code).
- Detail page: a **Workload** section showing resources, anti-affinity, and any labels/annotations/nodeSelector; a **Workload** dialog to edit resources (4 inputs), anti-affinity (3-way), and labels/annotations as key–value rows (same pattern as the config editor).
- i18n `workload.*`.

### W4: e2e + docs
- `scripts/workload-e2e.sh` + `make workload-e2e`: create with `{"workload":{"resources":{"cpu_request":"100m","memory_request":"256Mi"},"labels":{"team":"payments"}}}` → assert pod has the label, the resource requests, and a `podAntiAffinity` on hostname → PATCH resources → assert pods roll with the new values → reject a reserved label and a storage_class change (422) → delete.
- `docs/user-guide.md`: "Placement and resources" section — what anti-affinity does and why preferred is the default, setting requests/limits, labels/annotations for cost allocation and scraping, private registries, dedicated node pools.

## Verification
1. `make lint test openapi-check ui-client-check ui-build ui-lint ui-test` green.
2. `make workload-e2e` green.
3. `make failover-e2e` still green.
