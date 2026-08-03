# Multi-namespace operation — operator mode stage B design

Date: 2026-08-03
Status: approved

## Goal

One pglord operator manages PostgresClusters in every namespace: cluster-wide
watches, ClusterRole RBAC, and a namespace-qualified REST API/UI. Stage B of
the v1.2 milestone, building on stage A
(`2026-08-03-crd-operator-mode-design.md`); stage C (K8s-RBAC auth) follows.

## Decisions (settled with user)

| Question | Decision |
|---|---|
| Namespace selection | All namespaces by default (ClusterRole + cluster-wide watches). |
| API/UI model | Namespace-qualified: `/api/v1/namespaces/{ns}/clusters/...`, `namespace` field on summaries/details, UI namespace switcher. Legacy unqualified paths alias to the operator's own namespace. |

## Scope flag

`PGLORD_ALL_NAMESPACES=true|false` (Config `all_namespaces: bool`, default
false). Only meaningful with `spec_backend=crd`; the server refuses to start
with `all_namespaces=true` and the configmap backend (log + exit — a
misconfiguration, not a mode). Standalone deployments change nothing.

## Engine fan-out: cluster-wide watches, per-object namespace

The four loops stop assuming one namespace. Mechanically: `Api::namespaced`
becomes `Api::all` (mirror ConfigMaps keep their label selector; CRs
unlabeled) when `all_namespaces`, and every per-item code path takes the
namespace from the object's own `metadata.namespace` instead of the loop's
fixed `ns`:

- **cr_sync** — watches CRs cluster-wide; each CR's mirror is applied in the
  CR's namespace.
- **reconciler** — cluster-wide labeled ConfigMap watch; `apply_cluster`
  already takes `ns` as a parameter, callers pass the item's. The `known`
  set (deletion bookkeeping) keys on `(namespace, name)`.
- **orchestrator** — same listing change; probe/CAS/apply already
  parameterized by ns. Failure counters key on `(namespace, name)`.
- **backup scheduler / backup runs** — run-history ConfigMaps live in the
  cluster's namespace; ns threads through `start_backup` and `backup_runs`.

Renderers and agents are untouched: every derived object already lands in the
namespace it is applied to, and instance RBAC is namespaced per cluster.

## SpecStore and API surface

`SpecStore` methods gain the namespace: `list_all() -> Vec<(String,
ClusterSpec)>` (namespace + spec), and `get/put/delete/primary/set_primary`
take `ns: &str`. Both stores hold a `kube::Client` and build namespaced Apis
per call; `ConfigMapSpecStore` keeps working for standalone (its ns = the
operator's own, callers pass it).

Routes (contract regenerated, UI client too):

- `GET /api/v1/clusters` — all clusters the operator manages; summaries gain
  `namespace`. In single-namespace mode this is just the own-ns list.
- `/api/v1/namespaces/{ns}/clusters[/{name}...]` — the full existing cluster
  endpoint tree (CRUD, switchover, restart, hibernate/wake, fence/unfence,
  backups, restore, rotate, required-secrets), namespace-qualified.
- Legacy `/api/v1/clusters/{name}...` paths remain as aliases resolving to
  the operator's own namespace — existing clients and the current UI keep
  working during the transition; handlers are shared (an extractor resolves
  the effective namespace).

`ClusterDetail` gains `namespace`. Problem codes unchanged.

## Metrics

Per-cluster series gain a `namespace` label (name alone collides across
namespaces): `instances`, `instances_ready`, `sync_degraded`, `hibernated`,
`reconcile_total`, `failover_total`, `backup_runs_total`, `rotations_total`.
`forget_cluster(ns, name)`. Dashboards note in the user guide.

## RBAC and deployment

- New `deploy/k8s/operator/` variant: ClusterRole (the existing Role's rules,
  cluster-scoped, plus `pglord.dev` CR + status rules) + ClusterRoleBinding +
  the same Deployment with `PGLORD_SPEC_BACKEND=crd` and
  `PGLORD_ALL_NAMESPACES=true`. The single-namespace files stay untouched.
- Chart: `clusterWide: true` value renders ClusterRole/ClusterRoleBinding
  instead of Role/RoleBinding and sets both env vars. Default false.
- Preflight: when `all_namespaces`, SSAR checks run with an empty namespace
  (cluster-wide semantics) so the Permissions page reflects the ClusterRole.

## UI

- Namespace switcher in the header (populated from the summaries' distinct
  namespaces, "all" default), namespace column in the cluster list, namespace
  badge on the detail page. All API calls go through the ns-qualified client
  methods; create dialog gains a namespace field (defaulted to the current
  switcher selection).

## Error handling

| Failure | Behavior |
|---|---|
| `all_namespaces` with configmap backend | refuse to start with a clear log line |
| RBAC too narrow (Role instead of ClusterRole) | watches fail per-namespace; preflight page shows the denied cluster-wide checks |
| Same cluster name in two namespaces | fully supported — everything keys on (ns, name); metrics carry both labels |
| Legacy path for a cluster outside the own namespace | 404 — aliases only cover the own namespace by design |

## Testing

- **Server:** store-trait signature tests compile-time; route aliasing unit
  tests (legacy path resolves to own ns) in `tests/http.rs` 503-style; pure
  helpers for (ns,name) keying.
- **E2E** (`scripts/multi-ns-e2e.sh`, `make multi-ns-e2e`): operator deployed
  cluster-wide → CRs in two extra namespaces (`team-a`, `team-b`) with the
  same cluster name → both Ready, distinct storage → API list shows both with
  namespaces → ns-qualified REST edit converges in the right namespace →
  failover in one namespace does not touch the other → delete namespace
  `team-b` → its cluster garbage-collects, `team-a` untouched.
- **Docs:** user-guide operator-mode section extended with cluster-wide
  install and the namespace-qualified API.

## Out of scope (deliberate)

Namespace allow/deny lists (config exists to add later; default is all) ·
per-namespace defaults/quotas · stage C auth · UI namespace creation ·
migrating legacy API consumers (aliases cover them).
