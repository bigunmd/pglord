# PostgresCluster CRD — operator mode stage A design

Date: 2026-08-03
Status: approved

## Goal

Make a `PostgresCluster` custom resource the way to declare clusters, so
kubectl and GitOps become first-class alongside the UI/REST API. Stage A of
the v1.2 operator-mode milestone; stages B (multi-namespace) and C
(K8s-RBAC-mapped auth) get their own specs and build on this one.

## Decisions (settled with user)

| Question | Decision |
|---|---|
| v1.2 scope | Full milestone, decomposed: A = CRD (this spec), B = multi-namespace, C = RBAC auth — sequential, each its own spec/plan. |
| Architecture | CR-fronted, ConfigMap-engined: the CR is the truth and is synced into the existing `<name>-spec` ConfigMap; reconciler, orchestrator, backup scheduler, agents and instance RBAC run unchanged. |
| Namespaces (B) | Cluster-wide watch by default. |
| Auth (C) | Kubernetes bearer tokens: TokenReview to authenticate, SubjectAccessReview per request to authorize. |

## Mode selection

`PGLORD_SPEC_BACKEND` = `configmap` (default) | `crd`, a `Config` field like
any other. `configmap` is byte-identical to today. `crd` changes exactly two
things in `main()`: the store handed to `ClusterOps` is a `CrdSpecStore`, and
one extra loop (the CR sync) is spawned. Reconciler, orchestrator and backup
scheduler start identically in both modes and keep watching ConfigMaps.

## The CRD

- Kind `PostgresCluster`, group `pglord.dev`, version `v1alpha1`, namespaced,
  short name `pgc`, categories `all`.
- Spec is `ClusterSpec` verbatim (the design doc's long-standing intent).
  That includes its snake_case field names (`storage_size`, `pg_hba`) —
  unconventional for Kubernetes, but one schema shared with the REST
  contract beats a camelCase translation layer. Status fields follow suit.
- `spec.name` becomes `#[serde(default)]` (empty string). The CR sync injects
  `metadata.name` when it is empty; a non-empty mismatch is surfaced as an
  `Invalid` status, never silently reconciled. The REST create path is
  unchanged: an omitted name still fails validation (empty is not an RFC1123
  label).
- Status subresource: `ready_replicas`, `primary`, `hibernated`,
  `phase` (`Progressing` | `Ready` | `Degraded` | `Hibernated` | `Invalid`),
  `message` (human text for `Invalid`/`Degraded`).
- Printer columns: READY (`.status.ready_replicas`), PRIMARY, PHASE, AGE.

### Schema plumbing

`pglord-core` gains an optional `crd` cargo feature: `schemars` and
`kube/derive` behind it, `JsonSchema` dual-derived next to the existing
`utoipa::ToSchema` on the spec types, and a `crd.rs` module defining the
`#[derive(CustomResource)]` wrapper. `pglord-server` enables the feature.
Standalone consumers of `pglord-core` (the agent) do not.

The manifest `deploy/crds/postgresclusters.pglord.dev.yaml` is a generated,
committed artifact: `pglord-server --dump-crd` writes it (from
`PostgresCluster::crd()`), `make crd` regenerates, `make crd-check` fails CI
on drift — the `openapi.yaml` discipline exactly.

## CR sync loop (`cr_sync.rs`)

The reconciler's signal-only watch pattern over `Api<PostgresCluster>`:
events wake the loop, work is always a full sync, `RESYNC_INTERVAL` as backup.
Per CR, in order:

1. **Name rule.** Effective spec = CR spec with `name` = `metadata.name` when
   empty. `spec.name != metadata.name` → status
   `Invalid: spec.name does not match metadata.name`, skip.
2. **Validate** with `ClusterSpec::validate()`. Invalid → status `Invalid`
   with the error text, skip — existing children are never touched by an
   invalid edit, matching the API path where validation rejects before store.
3. **Adoption guard.** If `<name>-spec` exists without an ownerReference to
   this CR, status `Invalid: name conflict with existing cluster`, skip. A
   ConfigMap-backed cluster can never be clobbered by creating a same-named
   CR.
4. **Mirror.** SSA-apply the spec ConfigMap (field manager `pglord`): same
   labels as today — so every downstream loop picks it up unchanged — plus
   `pglord.dev/internal: "true"` for humans, and an ownerReference to the CR.
   GC chain: CR → spec ConfigMap → StatefulSet/Services/Secret/RBAC.
   The primary-record annotation on the mirror is engine state and is never
   written by the sync (SSA apply with a different field manager owner set
   leaves `metadata.annotations` held by the record's CAS writer untouched;
   the sync's apply object simply carries no annotations).
5. **Status.** Read the mirror's record annotation, the StatefulSet's
   `readyReplicas` and the spec to compute
   `phase`: `Invalid` (above) · `Hibernated` (`spec.hibernated`) ·
   `Ready` (`ready == replicas`) · `Degraded` (`0 < ready < replicas`, or
   sync-degraded) · `Progressing` (otherwise). Written via the status
   subresource only when changed, to avoid hot-looping on resourceVersion
   bumps.

## CrdSpecStore

Implements the existing six-method `SpecStore` over `Api<PostgresCluster>`:

- `list`/`get` — CRs by label-free namespaced list; `get` returns the
  effective spec (name injected) and an `OwnerRef` minted from the CR
  (`api_version: "pglord.dev/v1alpha1"`, kind, name, uid) — used only by the
  restore path's backup-runs ConfigMap and the template-diff, both inert to
  the value.
- `put` — SSA-apply the CR spec. UI edits land on the CR; kubectl sees them;
  `kubectl apply` users see `pglord` as a second field manager, which is
  correct SSA behavior.
- `delete` — delete the CR; GC does the rest.
- `primary`/`set_primary` — unchanged semantics, on the **mirror ConfigMap**
  (name is `<cluster>-spec`, namespace known), so the orchestrator's CAS path
  is byte-identical in both modes.

The API layer, problem codes, contract and UI need zero changes: the UI works
against CRD mode on day one. One wrinkle: `put` on a not-yet-synced CR means
the mirror may lag an API edit by up to one sync pass (~seconds); the API
already treats the reconciler as eventually consistent, so nothing new.

## Deployment

- `deploy/crds/` manifest (generated). Installing a CRD is the one
  cluster-scoped act; standalone mode still needs none of it.
- `deploy/k8s/rbac.yaml` and the chart gain, **only in crd mode** (chart:
  `values.specBackend=crd`; raw: a commented block), rules for
  `pglord.dev/postgresclusters` (get/list/watch/create/patch/delete) and
  `postgresclusters/status` (get/patch). Namespaced Role — stage A stays
  single-namespace.
- `PGLORD_SPEC_BACKEND` env plumbed through deployment.yaml + chart values.
- Preflight (`preflight.rs::required()`) gains the two CR rules when the
  backend is `crd`, so the Permissions page stays honest.

## Error handling

| Failure | Behavior |
|---|---|
| Invalid CR spec (kubectl bypasses API validation) | status `Invalid` + message; children untouched; CRD structural schema catches type errors earlier |
| CR name collides with ConfigMap-backed cluster | status `Invalid: name conflict`, no adoption, no overwrite |
| spec.name ≠ metadata.name | status `Invalid`, skip |
| Mirror CM edited by hand | next sync SSA-forces it back to the CR spec |
| CRD not installed but backend=crd | server logs the list error each pass and the API answers 500 `internal` on store calls; preflight shows the missing permission — documented, no special code path |
| Status write conflict | best-effort; next pass rewrites |

## Testing

- **Core:** `crd` feature compiles standalone and with the feature;
  `PostgresCluster::crd()` snapshot sanity (group/version/kind, subresource
  present, printer columns); `spec.name` default keeps REST validation
  behavior (empty name still invalid).
- **Server:** `--dump-crd` writes a parseable manifest; drift check target;
  pure sync helpers unit-tested (phase computation, name rule, adoption
  guard decision) — table tests, no kube.
- **E2E** (`scripts/operator-e2e.sh`, `make operator-e2e`): deploy with
  `PGLORD_SPEC_BACKEND=crd` + CRD installed → `kubectl apply` a
  PostgresCluster → cluster reaches Ready, status/printer columns populated →
  edit replicas via kubectl → converges → edit a GUC via the REST API →
  visible in `kubectl get pgc -o yaml` → failover still works (record on
  mirror) → hibernate via API → status phase `Hibernated` → wake →
  `kubectl delete pgc` → children and PVCs garbage-collected. Coexistence:
  a ConfigMap-backed cluster next to a CR-backed one; same-name CR is
  rejected via status.
- **Docs:** user-guide "Operator mode" section (install CRD, backend flag,
  kubectl examples, coexistence rules).

## Out of scope (this stage)

Multi-namespace (stage B) · RBAC-mapped auth (stage C) · CRD conversion
webhooks/versioning beyond `v1alpha1` · admission webhooks (validation is
status-surfaced) · migration tooling from ConfigMap-backed clusters (both
modes coexist; migration = recreate) · Helm CRD lifecycle management beyond
installing the manifest.
