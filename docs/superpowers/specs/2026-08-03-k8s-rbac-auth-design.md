# Kubernetes-RBAC-mapped auth — operator mode stage C design

Date: 2026-08-03
Status: approved

## Goal

Let Kubernetes RBAC be pglord's authorization system: a user presents a
Kubernetes bearer token (`kubectl create token`), pglord authenticates it
with TokenReview and authorizes every request with a SubjectAccessReview
against `pglord.dev/postgresclusters` in the target namespace. Whoever may
`kubectl patch pgc` in a namespace may do the same through the UI — one
permission model, no second user database. Stage C of v1.2, on top of stages
A (CRD) and B (multi-namespace).

## Decisions (settled with user)

| Question | Decision |
|---|---|
| Identity | Kubernetes bearer tokens via TokenReview; no new identity infrastructure. |
| Authorization | SubjectAccessReview per request, mapped to verbs on `pglord.dev/postgresclusters`. |
| Existing modes | Static tokens and OIDC remain; `kubernetes` is a third, independently toggleable mode. |

## Mode and configuration

`PGLORD_AUTH_KUBERNETES=true` (Config `auth_kubernetes: bool`, default
false). Any enabled mode turns `auth_enabled()` on. Evaluation order in the
middleware: static token match → OIDC validation → TokenReview — the first
mode that positively authenticates wins; kubernetes mode is the fallback for
bearer tokens the other modes do not recognize.

Requires a Kubernetes environment (the mode is inert without one, like
cluster management itself) and two delegated permissions for the server's
ServiceAccount, both cluster-scoped:

- `authentication.k8s.io/tokenreviews: create`
- `authorization.k8s.io/subjectaccessreviews: create`

The standard `system:auth-delegator` ClusterRole grants exactly these; the
operator RBAC binds it. Preflight gains both checks when the mode is on.

## Request mapping (pure, unit-tested)

The alias middleware has already normalized every cluster path to
`/api/v1/namespaces/{ns}/clusters...`, so the mapping sees one shape:

| Request | SAR |
|---|---|
| `GET .../namespaces/{ns}/clusters` | `list postgresclusters` in ns |
| `GET .../namespaces/{ns}/clusters/{name}[/backups|/secrets]` | `get` in ns |
| `POST .../namespaces/{ns}/clusters` | `create` in ns |
| `PATCH .../clusters/{name}` | `patch` in ns |
| `DELETE .../clusters/{name}` | `delete` in ns |
| Day-2 POST (`switchover`, `restart`, `hibernate`, `wake`, `fence`, `unfence`, `backups`, `rotate-credentials`) | `patch` in ns |
| `POST .../{name}/restore` | `create` in ns (it makes a new cluster) |
| `GET /api/v1/clusters` (global list) | `list postgresclusters` with no namespace (cluster-wide) — users without cluster-wide list use the per-namespace list |
| Other `/api/v1/*` (ping, version, gucs, preflight, auth/config public as today) | authenticated is enough — they expose no cluster data |

Verbs are checked on `group: pglord.dev, resource: postgresclusters`
regardless of the storage backend: they are the API's nouns. In configmap
mode admins grant Roles on the same nouns even though no CRs exist — RBAC
does not require the resource to exist.

## TokenReview caching

A TokenReview round-trip per request is too slow for a UI polling every 5s.
In-memory cache keyed by SHA-256 of the token: authenticated →
`(username, groups)` for 60 seconds or the token's reported expiry,
whichever is sooner; failures are not cached. SAR results are cached the
same way, keyed by (token hash, verb, ns) — 10 seconds, so a permission
revocation takes effect within seconds while the steady-state cost is one
SAR per (user, namespace, verb) per 10s.

## Errors

- No/invalid token → existing 401 `auth_required` (WWW-Authenticate: Bearer).
- Authenticated but SAR denied → new 403 problem code `forbidden`, detail
  naming the verb and namespace ("cannot patch postgresclusters in team-a").

## UI

The login page's existing token form works as-is — the user pastes the
output of `kubectl create token <sa>`. `GET /api/v1/auth/config` gains
`kubernetes_enabled: bool`; when true the login page shows a hint line with
the `kubectl create token` recipe. No other UI changes: 403s surface through
the existing problem-detail toast path.

## Deployment

- Operator RBAC (`deploy/k8s/operator/rbac.yaml`): add a ClusterRoleBinding
  of `system:auth-delegator` to the pglord-server ServiceAccount, in a
  clearly commented block.
- Chart: `auth.kubernetes: false` value → env + the auth-delegator binding
  when true (requires `clusterWide` RBAC objects or standalone install with
  the extra binding — the binding is cluster-scoped either way).
- Raw manifests: commented env line like the other auth modes.
- User guide: "Kubernetes-native auth" section with a full recipe — create
  SA, grant a Role on postgresclusters, `kubectl create token`, paste into
  the login page; note the global list needs cluster-wide `list`.

## Testing

- **Unit:** the mapping function (every row of the table above, plus
  non-API and public paths); cache TTL behavior with a fake clock.
- **Integration (`tests/http.rs`):** kubernetes mode on without kube env →
  bearer tokens answer 401 (TokenReview unavailable = unauthenticated, never
  a 500); auth-config reports `kubernetes_enabled`.
- **E2E** (`scripts/rbac-auth-e2e.sh`, `make rbac-auth-e2e`): operator mode +
  kubernetes auth on → SA `alice` with get/list/patch on postgresclusters in
  team-a only → `kubectl create token alice` → API list in team-a works,
  PATCH in team-a works (202/200), GET in team-b → 403 `forbidden`, global
  list → 403, DELETE in team-a → 403 (no delete verb) → static admin token
  still works everywhere. Reset auth afterwards (per the e2e harness
  pitfalls memory: `|| true` on get-by-name pipelines, no `|| echo`).

## Out of scope (deliberate)

Impersonation-based OIDC mapping · per-instance/sub-resource granularity ·
audit logging beyond existing tracing · UI role management · caching
invalidation webhooks · group-based UI feature flags.
