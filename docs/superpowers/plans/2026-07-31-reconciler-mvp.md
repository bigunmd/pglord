# pglord Reconciler MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A cluster created through the REST API materializes as a StatefulSet of agent-managed Postgres pods with a streaming primary+replica topology, reconciled level-triggered from spec ConfigMaps — proven end-to-end on the minikube stand.

**Architecture:** Specs live as labeled ConfigMaps (`SpecStore` trait so operator mode can swap CRDs in later). A kube-rs watcher + requeue loop renders desired objects (`pglord-core` renderer, pure functions over `k8s-openapi` types) and applies them with server-side apply (field manager `pglord`). Deletion is garbage-collected via ownerReferences from every rendered object to its spec ConfigMap. Role assignment is topology-derived for MVP: agents run with `role=auto` and self-select primary (ordinal 0) vs standby (ordinal >0, streaming from pod-0's stable DNS name); real failover orchestration is the next slice. The `-rw` Service pins pod-0 via the `statefulset.kubernetes.io/pod-name` label for now.

**Tech Stack:** kube 2.x (client, runtime, derive off), k8s-openapi (latest k8s feature), serde_json; existing axum/utoipa pipeline; ng-openapi-gen for the TS client.

## Global Constraints

- Spec ConfigMaps labeled `app.kubernetes.io/managed-by=pglord`, `pglord.dev/kind=cluster-spec`; JSON spec under data key `spec`.
- All rendered objects carry the same managed-by label plus `app.kubernetes.io/instance=<cluster>` and an ownerReference to the spec ConfigMap (GC handles delete).
- Server-side apply only (`Patch::Apply`, field manager `pglord`, force=true); the reconciler must be idempotent and level-triggered.
- Cluster names: RFC1123 label (`^[a-z0-9]([-a-z0-9]{0,38}[a-z0-9])?$`, ≤40 chars — leaves room for suffixes). Replicas 1–5.
- `spec/openapi.yaml` and `ui/src/app/api` are regenerated and committed in the same commit as any API change (CI drift gates).
- Every commit keeps `make lint && make test` green.

---

### Task R1: ClusterSpec model in pglord-core

**Files:** `crates/pglord-core/Cargo.toml` (serde, serde_json, thiserror), `crates/pglord-core/src/spec.rs`, `src/lib.rs`.

**Produces:**
```rust
pub struct ClusterSpec { pub name: String, pub image: String /* default "pglord-agent:dev" */,
    pub replicas: u8 /* default 1 */, pub storage_size: String /* default "1Gi" */ }
pub enum SpecError { InvalidName(String), InvalidReplicas(u8), InvalidStorage(String) }
impl ClusterSpec { pub fn validate(&self) -> Result<(), SpecError>; }
```

- [ ] Tests first: valid spec passes; uppercase/underscore/41-char names rejected; replicas 0 and 6 rejected; storage `"1Gi"`/`"500Mi"` ok, `"abc"` rejected (regex `^[0-9]+(Ki|Mi|Gi|Ti)$`).
- [ ] Implement with serde defaults (`#[serde(default = ...)]`) so `{"name":"x"}` deserializes to a full spec.
- [ ] Commit `feat(core): ClusterSpec model with validation`.

### Task R2: Agent auto-role + /readyz

**Files:** `crates/pglord-agent/src/config.rs` (Role::Auto), `src/rolestrategy.rs` (new), `src/http.rs` (/readyz), `src/main.rs` (resolve before bootstrap).

**Produces:** `resolve_role(role: Role, hostname: &str) -> anyhow::Result<Role>` — `Auto` + hostname `foo-0` → `Primary`; `Auto` + `foo-3` → `Standby`; explicit roles pass through; `Auto` + non-ordinal hostname → error. `GET /readyz` → 200 iff `status.ready`, else 503 (used as the pod readinessProbe).

- [ ] Tests first (ordinal parsing incl. `a-b-2`, `foo`, `foo-`); then implement; standby with `Auto` keeps `primary_host` from env (reconciler sets it).
- [ ] Commit `feat(agent): auto role by pod ordinal and /readyz probe`.

### Task R3: Object renderer in pglord-core

**Files:** `crates/pglord-core/Cargo.toml` (+k8s-openapi latest feature), `crates/pglord-core/src/render.rs`.

**Produces:**
```rust
pub struct OwnerRef { pub api_version: String, pub kind: String, pub name: String, pub uid: String }
pub fn render_statefulset(spec: &ClusterSpec, ns: &str, owner: &OwnerRef) -> StatefulSet;
pub fn render_headless_service(spec, ns, owner) -> Service;  // "<name>-pods", clusterIP None
pub fn render_rw_service(spec, ns, owner) -> Service;        // "<name>-rw" -> pod-name=<name>-0
pub fn labels(name: &str) -> BTreeMap<String, String>;
```
StatefulSet details: serviceName `<name>-pods`; container `postgres` image `spec.image`; env `PGLORD_AGENT_ROLE=auto`, `PGLORD_AGENT_PRIMARY_HOST=<name>-0.<name>-pods.<ns>.svc.cluster.local`, `RUST_LOG=info`; ports 5432+8008; readinessProbe httpGet `/readyz:8008`, livenessProbe `/healthz:8008`; volumeClaimTemplate `data` of `storage_size` mounted at `/var/lib/postgresql/data`.

- [ ] Tests first: sts replicas/serviceName/env/probe paths/pvc size; services selectors incl. `-rw` pod pin; every object has owner ref + labels.
- [ ] Commit `feat(core): k8s object renderer for cluster topology`.

### Task R4: Kube client + ConfigMap SpecStore

**Files:** `crates/pglord-server/Cargo.toml` (+kube, k8s-openapi, futures), `src/kube.rs` (client init + `PROBE_NAME="kube"` health loop pinging the apiserver `/version`), `src/spec_store.rs`.

**Produces:**
```rust
#[async_trait::async_trait]
pub trait SpecStore: Send + Sync {
    async fn list(&self) -> anyhow::Result<Vec<ClusterSpec>>;
    async fn get(&self, name: &str) -> anyhow::Result<Option<(ClusterSpec, OwnerRef)>>;
    async fn put(&self, spec: &ClusterSpec) -> anyhow::Result<()>;   // create-or-update CM "<name>-spec"
    async fn delete(&self, name: &str) -> anyhow::Result<bool>;
}
pub struct ConfigMapSpecStore { api: Api<ConfigMap> }
```
`put` SSA-applies the ConfigMap (labels per Global Constraints, `data.spec` = JSON). `get` returns the CM's uid as OwnerRef for rendering.

- [ ] Unit-test the pure parts (CM->spec parse round-trip via constructing a ConfigMap value); kube-dependent paths covered by the e2e (R7).
- [ ] main.rs: init client only when kube env detected? No — always init (lazy client); register `kube` probe; store + reconciler spawn behind successful client creation, logged warning otherwise (local `make run` without kubeconfig still serves API with 503 readiness).
- [ ] Commit `feat(server): kube client and ConfigMap-backed SpecStore`.

### Task R5: Reconcile loop

**Files:** `crates/pglord-server/src/reconciler.rs`, `src/main.rs` (spawn).

**Produces:** `pub async fn run(client: kube::Client, ns: String, readiness: Readiness)` — kube-rs `watcher` on ConfigMaps with the spec labels, debounced into a work queue; on each event (and a 60s periodic resync) for every spec: parse, validate, render (R3), SSA-apply StatefulSet + both Services. Apply errors: log + requeue after 10s; never crash the loop.

- [ ] Unit-test `desired_objects(spec, ns, owner) -> (StatefulSet, Service, Service)` composition fn; the loop itself is e2e-tested.
- [ ] Commit `feat(server): level-triggered reconcile loop with server-side apply`.

### Task R6: Clusters API + regenerated clients

**Files:** `crates/pglord-server/src/api/clusters.rs`, `src/api/mod.rs`, `src/app.rs` (routes + AppState store/kube handles), `spec/openapi.yaml`, `ui/src/app/api/**` (regen).

**Produces (all utoipa-annotated, tag `clusters`):**
- `POST /api/v1/clusters` body `ClusterSpecDto` → 201 (validate → 422 `invalid_spec` problem on failure; 409 `already_exists`)
- `GET /api/v1/clusters` → `[ClusterSummary { name, image, replicas, ready_replicas: i32 }]` (ready from StatefulSet status; 0 if absent)
- `GET /api/v1/clusters/{name}` → `ClusterSummary` or 404
- `DELETE /api/v1/clusters/{name}` → 204 or 404
Store is `Option<Arc<dyn SpecStore>>` in AppState → 503 `kube_unavailable` problem when absent (local no-kube runs).

- [ ] Integration tests in `tests/http.rs` for the no-kube path: POST → 503 problem+json `kube_unavailable`; OpenAPI contains `/api/v1/clusters`.
- [ ] `make openapi && make ui-client` — commit regenerated artifacts together with the code: `feat(server): clusters CRUD API`.

### Task R7: Minikube e2e

**Files:** `scripts/cluster-e2e.sh`, `Makefile` (`cluster-e2e`), `deploy/k8s/deployment.yaml` (env `PGLORD_NAMESPACE` via fieldRef if needed).

- [ ] Script: `make stand-image` (server) + `make agent-image` + `minikube -p pglord image load pglord-agent:dev`; `kubectl apply` manifests; port-forward server; `curl -X POST /api/v1/clusters -d '{"name":"demo","replicas":2}'`; poll `GET /api/v1/clusters/demo` until `ready_replicas==2` (timeout 180s); `kubectl exec demo-0 psql -c "CREATE TABLE t(x int); INSERT 42"`; poll psql on `demo-1` until row visible; `DELETE /api/v1/clusters/demo`; poll until StatefulSet gone (GC via ownerReference). Trap cleanup.
- [ ] Run green; commit `feat: minikube e2e — cluster via REST API with streaming replication`.

## Verification

1. `make lint && make test` clean; `make openapi-check ui-client-check` clean.
2. `make agent-e2e` still green (explicit-role path unchanged).
3. `make cluster-e2e`: API-created cluster reaches 2/2 ready, row written on `demo-0` readable on `demo-1`, DELETE garbage-collects everything.
