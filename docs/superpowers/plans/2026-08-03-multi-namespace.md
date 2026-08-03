# Multi-Namespace Operation (Operator Mode Stage B) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One operator managing PostgresClusters in every namespace — cluster-wide watches, ClusterRole RBAC, namespace-qualified API/UI — per `docs/superpowers/specs/2026-08-03-multi-namespace-design.md`.

**Architecture:** `PGLORD_ALL_NAMESPACES` flips the four loops from `Api::namespaced` to `Api::all` with per-object namespace threading; `SpecStore` gains ns parameters; the REST tree moves under `/api/v1/namespaces/{ns}/...` with a URI-rewrite middleware aliasing legacy paths to the operator's own namespace; per-cluster metrics gain a `namespace` label.

**Tech Stack:** existing axum/utoipa/kube-rs stack; no new dependencies.

## Global Constraints

- `all_namespaces=true` is valid only with `spec_backend=crd`; the server refuses to start otherwise.
- Legacy `/api/v1/clusters...` paths keep working as aliases to the operator's own namespace (URI rewrite); the OpenAPI contract documents only the namespace-qualified tree.
- Everything keys on `(namespace, name)` — same cluster name in two namespaces is fully supported.
- Contract regenerated (`make openapi ui-client`), CRD manifest unchanged.
- Single-namespace standalone behavior stays byte-identical when the flag is off.
- Clippy `-D warnings`; `Config::test_default()` gains any new field; both install paths change together.

Counted sweep sites: 22 `ops.store.*` calls (16 clusters.rs, 2 each backups/secrets/restore), 12 `Path(name): Path<String>` handlers, 20 `with_label_values` sites.

---

### Task 1: Config flag + startup validation

**Files:**
- Modify: `crates/pglord-server/src/config.rs`, `crates/pglord-server/src/main.rs`

**Interfaces:**
- Produces: `Config.all_namespaces: bool` (serde default false).

- [ ] **Step 1:** Field:

```rust
    /// Operator mode only: watch and manage PostgresClusters in every
    /// namespace (needs ClusterRole RBAC). Invalid with the configmap
    /// backend — the server refuses to start.
    #[serde(default)]
    pub all_namespaces: bool,
```

plus `all_namespaces: false,` in `test_default()`.

- [ ] **Step 2:** In `main()` right after `Config::load()`:

```rust
    if cfg.all_namespaces && cfg.spec_backend != "crd" {
        anyhow::bail!(
            "PGLORD_ALL_NAMESPACES=true requires PGLORD_SPEC_BACKEND=crd; \
             the configmap backend is single-namespace by design"
        );
    }
```

- [ ] **Step 3:** Config unit test in `config.rs` tests (figment Jail pattern used by the existing tests): set both env vars, assert `Config::load()` parses `all_namespaces: true`; the bail itself is main-only glue.
- [ ] **Step 4:** `cargo test -p pglord-server config` → PASS; commit `feat(server): all_namespaces flag, crd-only validation`.

---

### Task 2: SpecStore namespace parameterization

**Files:**
- Modify: `crates/pglord-server/src/spec_store.rs`, `src/crd_store.rs`, and every `ops.store.` call site (16 in `api/clusters.rs`, 2 each in `api/backups.rs`, `api/secrets.rs`, `api/restore.rs`)

**Interfaces:**
- Produces (later tasks depend on these exact signatures):

```rust
#[async_trait::async_trait]
pub trait SpecStore: Send + Sync {
    /// Every cluster this store manages, with its namespace.
    async fn list_all(&self) -> anyhow::Result<Vec<(String, ClusterSpec)>>;
    async fn get(&self, ns: &str, name: &str) -> anyhow::Result<Option<(ClusterSpec, OwnerRef)>>;
    async fn put(&self, ns: &str, spec: &ClusterSpec) -> anyhow::Result<()>;
    async fn delete(&self, ns: &str, name: &str) -> anyhow::Result<bool>;
    async fn primary(&self, ns: &str, name: &str) -> anyhow::Result<Option<String>>;
    async fn set_primary(&self, ns: &str, name: &str, target: &str) -> anyhow::Result<bool>;
}
```

- [ ] **Step 1:** Rewrite both stores to hold `client: kube::Client` (+ `own_ns: String` for `list_all` in the ConfigMap store; + `all_namespaces: bool` in the CRD store) and build `Api::namespaced(self.client.clone(), ns)` per call. `ConfigMapSpecStore::list_all` lists its own namespace only (standalone semantics); `CrdSpecStore::list_all` uses `Api::<PostgresCluster>::all` when `all_namespaces`, else namespaced, and maps each CR to `(cr.metadata.namespace.unwrap_or_default(), effective_spec(&cr))`. `CrdSpecStore::new(client, own_ns, all_namespaces)`.
- [ ] **Step 2:** Mechanical sweep: every `ops.store.X(&name, ...)` call gains the namespace argument. Until Task 3 lands the route change, handlers pass `&ops.ns` — the compiler is the checklist (`cargo check -p pglord-server` until clean). `list_clusters` switches to `list_all()` and drops the namespace for now (`.map(|(_, spec)| spec)`).
- [ ] **Step 3:** `crd_store.rs` unit test extends: `effective_spec` untouched; add none needed beyond compile — the store logic is I/O. `cargo test -p pglord-server` → PASS (76+).
- [ ] **Step 4:** Commit `refactor(server): SpecStore keyed by namespace + name`.

---

### Task 3: Namespace-qualified routes, alias middleware, DTO fields

**Files:**
- Modify: `crates/pglord-server/src/app.rs`, all handler files under `src/api/`, `crates/pglord-server/tests/http.rs`

**Interfaces:**
- Produces: every cluster-tree handler takes `Path((ns, name)): Path<(String, String)>` (or `Path(ns)` for list/create) with utoipa paths `/api/v1/namespaces/{ns}/clusters...`; `ClusterSummary.namespace`, `ClusterDetail.namespace`; middleware `alias_legacy_paths`.

- [ ] **Step 1: Failing test** in `tests/http.rs`:

```rust
#[tokio::test]
async fn legacy_cluster_paths_alias_to_the_own_namespace() {
    // Without kube both forms must resolve to the same handler → same 503.
    for path in [
        "/api/v1/clusters/demo",
        "/api/v1/namespaces/pglord/clusters/demo",
    ] {
        let (status, body) = send(test_app(), path, None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(body["code"], "kube_unavailable", "{path}");
    }
}
```

- [ ] **Step 2:** Run → FAIL (ns path 404).
- [ ] **Step 3:** Implement.

Middleware in `app.rs` (before the router):

```rust
/// Legacy unqualified cluster paths keep working by rewriting them onto the
/// operator's own namespace before routing. The OpenAPI contract documents
/// only the namespace-qualified tree.
async fn alias_legacy_paths(
    axum::extract::State(own_ns): axum::extract::State<String>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path();
    if let Some(rest) = path.strip_prefix("/api/v1/clusters") {
        let new = format!("/api/v1/namespaces/{own_ns}/clusters{rest}");
        let mut parts = req.uri().clone().into_parts();
        parts.path_and_query = Some(
            match req.uri().query() {
                Some(q) => format!("{new}?{q}"),
                None => new,
            }
            .parse()
            .expect("rewritten uri"),
        );
        if let Ok(uri) = axum::http::Uri::from_parts(parts) {
            *req.uri_mut() = uri;
        }
    }
    next.run(req).await
}
```

applied with `.layer(axum::middleware::from_fn_with_state(own_ns.clone(), alias_legacy_paths))` on the api router (own_ns from `ClusterOps.ns`, or the config namespace when ops is None — thread a plain `String` into `app::build`; test builds pass `"pglord"` via `Config::test_default()`-derived value: add `namespace: String` to Config with default `"pglord"`, plumbed from `kube_client` resolution in main).

Handler sweep (the 12 `Path(name)` handlers + create/list): path annotation `path = "/api/v1/namespaces/{ns}/clusters/{name}/..."`, params gain `("ns" = String, Path, description = "Namespace")`, extractor `Path((ns, name)): Path<(String, String)>`, every `&ops.ns` from Task 2 becomes `&ns`. `create_cluster`/`list_clusters` move under `/api/v1/namespaces/{ns}/clusters` with `Path(ns): Path<String>`; a separate `GET /api/v1/clusters` (`list_all_clusters`) returns all managed clusters. Instance-scoped fence/unfence become `/api/v1/namespaces/{ns}/clusters/{name}/instances/{pod}/...` with `Path((ns, name, pod))`.

DTOs: `pub namespace: String` on `ClusterSummary` and `ClusterDetail` (doc: "Namespace the cluster lives in"), filled from the handler's ns (`summarize(ops, ns, spec)`).

Background spawns in handlers (`roll_pods`, rotation, restore) capture the request ns instead of `ops.ns` — grep `ops.ns` inside `api/` until only the middleware default remains.

- [ ] **Step 4:** Extend the 503 route test's list with the ns-qualified forms of the four hibernation routes (regression that both trees route). `cargo test -p pglord-server` → PASS; `make lint`.
- [ ] **Step 5:** Commit `feat(server): namespace-qualified cluster API with legacy alias middleware`.

---

### Task 4: Engine fan-out + metrics namespace label

**Files:**
- Modify: `crates/pglord-server/src/cr_sync.rs`, `src/reconciler.rs`, `src/orchestrator.rs`, `src/backup_scheduler.rs`, `src/backup_runs.rs`, `src/api/backups.rs` (start_backup ns), `src/metrics.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `Config.all_namespaces` (Task 1).
- Produces: `run(client, ns, all_namespaces, ...)` signatures on the four loops; `Metrics` per-cluster vecs labeled `["namespace", "cluster"]`; `forget_cluster(ns, name)`.

- [ ] **Step 1:** `metrics.rs`: `gauge_vec`/`counter_vec` label arrays become `&["namespace", "cluster"]` (plus existing extra labels appended: `reconcile_total` → `["namespace", "cluster", "outcome"]`, `failover_total` → `["namespace", "cluster", "trigger"]`, `backup_runs_total` → `["namespace", "cluster", "kind", "outcome"]`, `rotations_total` → `["namespace", "cluster", "outcome"]`). `forget_cluster(&self, ns: &str, cluster: &str)` updates removals. Compiler-driven sweep of all 20 `with_label_values` sites to prepend the namespace.
- [ ] **Step 2:** Loop changes, one shape four times — `Api::namespaced` → conditional:

```rust
    let cms: Api<ConfigMap> = if all_namespaces {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &ns)
    };
```

(label selector unchanged — mirrors and standalone specs share it), and inside each per-item body the namespace comes from the object:

```rust
    let item_ns = cm.metadata.namespace.clone().unwrap_or_else(|| ns.clone());
```

threading `&item_ns` into `observe_cluster`, `apply_cluster`, `cas_set_primary`'s Api (construct `Api::namespaced(client.clone(), &item_ns)` for the CAS patch when all_namespaces), `start_backup`, `backup_runs::{list, append}` and the run-history ConfigMap owner. Keying: `failures: HashMap<(String, String), u32>` in the orchestrator; `known: HashSet<(String, String)>` in the reconciler (deletion cleanup calls `forget_cluster(&ns, &name)` and `delete_orphan_pvcs` in the right ns).
- [ ] **Step 3:** `cr_sync::run(client, ns, all_namespaces)` — CR watch via `Api::all` under the flag; `sync_one` already takes ns per call; pass the CR's own namespace.
- [ ] **Step 4:** `main.rs` passes `cfg.all_namespaces` to all four spawns and to `CrdSpecStore::new`.
- [ ] **Step 5:** Unit tests: orchestrator election tests unchanged (pure); add a metrics smoke test asserting the new label arity:

```rust
    #[test]
    fn per_cluster_series_carry_namespace() {
        let m = Metrics::new();
        m.instances.with_label_values(&["team-a", "demo"]).set(2);
        assert!(m.render().contains(r#"pglord_cluster_instances{cluster="demo",namespace="team-a"}"#));
        m.forget_cluster("team-a", "demo");
        assert!(!m.render().contains(r#"namespace="team-a""#));
    }
```

- [ ] **Step 6:** `cargo test -p pglord-server && make lint` → PASS. Commit `feat(server): cluster-wide watches with per-object namespace; metrics gain a namespace label`.

---

### Task 5: Contract + UI namespace awareness

**Files:**
- Regenerate: `spec/openapi.yaml`, `ui/src/app/api/**`
- Modify: `ui/src/app/pages/clusters/clusters.component.ts`, `cluster-detail.component.ts`, `cluster-create-dialog.component.ts`, app shell (header) component, `ui/public/i18n/en.json`

- [ ] **Step 1:** `make openapi ui-client`; commit regenerated artifacts together with the UI changes at the end of this task.
- [ ] **Step 2:** Namespace state: a small `NamespaceStore` service (signal persisted to `localStorage['pglord.namespace']`, default `""` = all) exposed to the header switcher; the switcher's options derive from the distinct `namespace` values of the last summaries load, plus "All".
- [ ] **Step 3:** Cluster list: use `listAllClusters` when "All" (namespace column shown) else the ns-qualified list; row navigation carries the namespace in the route (`/namespaces/:ns/clusters/:name` route param added alongside the legacy route which assumes own ns).
- [ ] **Step 4:** Detail component: resolve ns from the route, thread into every generated client call (the regenerated fns take `ns`), show a namespace badge next to the title.
- [ ] **Step 5:** Create dialog: namespace input defaulted from the switcher (hidden when the API reports single-namespace mode — expose `all_namespaces` on the public `/api/v1/auth/config`-style info? Simpler: always show, prefilled with the switcher's ns or the first summary's ns).
- [ ] **Step 6:** i18n keys `clusters.namespace`, `clusters.allNamespaces`, `clusterDetail.namespace`. `make ui-lint ui-build ui-test` → clean. Commit `feat(ui): namespace switcher, namespaced routes and cluster list column` + `chore(contract): regenerate openapi + ts client for namespaces`.

---

### Task 6: RBAC operator variant, chart, preflight

**Files:**
- Create: `deploy/k8s/operator/rbac.yaml` (ClusterRole + ClusterRoleBinding), `deploy/k8s/operator/README.md` (3 lines: apply crds/, apply this dir over the base, set both env vars)
- Modify: `charts/pglord/templates/rbac.yaml`, `charts/pglord/values.yaml`, `charts/pglord/templates/deployment.yaml`, `crates/pglord-server/src/preflight.rs` + callers

- [ ] **Step 1:** `deploy/k8s/operator/rbac.yaml`: ClusterRole with the existing Role's exact rule set plus both `pglord.dev` rules; ClusterRoleBinding to `pglord-server` SA in ns `pglord`.
- [ ] **Step 2:** Chart: `clusterWide: false` value; when true, `rbac.yaml` template renders ClusterRole/ClusterRoleBinding (same rules) instead of Role/RoleBinding, deployment sets `PGLORD_ALL_NAMESPACES: "true"`. `helm template --set config.specBackend=crd --set clusterWide=true` sanity grep.
- [ ] **Step 3:** Preflight: `run(client, ns, spec_backend, all_namespaces)`; when `all_namespaces` the SSAR `ResourceAttributes.namespace` is `None` (cluster-wide check). Callers (main.rs, api/preflight.rs via a new `ClusterOps.all_namespaces: bool`) updated. Test: `required()` unchanged; add an SSAR-shape unit test only if one exists for the ns already — otherwise the e2e covers it.
- [ ] **Step 4:** `cargo test -p pglord-server && make lint` → PASS. Commit `feat(deploy): cluster-wide operator RBAC variant; preflight checks cluster-wide`.

---

### Task 7: Multi-namespace e2e + docs

**Files:**
- Create: `scripts/multi-ns-e2e.sh`
- Modify: `Makefile`, `docs/user-guide.md`

- [ ] **Step 1:** Script (copy the operator-e2e harness: image build + stale-image check, port 18098, per the e2e-shell-harness pitfalls memory — no `|| echo 0`, `|| true` on get-by-name pipelines, cordon for any failover step). Flow:
  1. Install CRDs; apply `deploy/k8s/operator/rbac.yaml`; set `PGLORD_SPEC_BACKEND=crd PGLORD_ALL_NAMESPACES=true`; rollout.
  2. `kubectl create ns team-a team-b`; apply a PostgresCluster named `demo` (replicas 1) in **both**.
  3. Both reach phase Ready; `GET /api/v1/clusters` lists two entries named demo with namespaces team-a/team-b.
  4. Write distinct marker rows in each; `PATCH /api/v1/namespaces/team-a/clusters/demo {"parameters":{"max_connections":"150"}}` → visible only on team-a's CR.
  5. Legacy alias check: `GET /api/v1/clusters/demo` → 404 (own ns has no demo) — proves aliases stay own-ns scoped.
  6. `kubectl delete ns team-b` → its cluster and PVCs garbage-collect; team-a's demo still Ready with its marker row.
  7. Cleanup: delete team-a ns, reset env to standalone.
- [ ] **Step 2:** Makefile `multi-ns-e2e` target; run `make multi-ns-e2e` on the stand.
- [ ] **Step 3:** User guide: extend the operator-mode section — cluster-wide install, namespace-qualified API examples, metrics label note.
- [ ] **Step 4:** Commit `test(e2e): multi-namespace operator flow` and `docs(user-guide): cluster-wide operator install`.

---

## Self-review notes

- Spec coverage: flag/validation → T1; store → T2; routes/alias/DTO → T3; fan-out + (ns,name) keys + metrics → T4; UI/contract → T5; RBAC/chart/preflight → T6; e2e/docs → T7. "Refuse to start" behavior verified manually in e2e step 1 implicitly (env set only with crd).
- Type consistency: trait signatures (T2) used by handlers (T3); `run(..., all_namespaces)` (T4) matches main.rs spawns; `forget_cluster(ns, name)` matches reconciler cleanup call; `ClusterOps.all_namespaces` introduced in T6 where first needed.
- Middleware alias keeps `pod_template_changed`'s synthetic owner untouched (value-inert).
