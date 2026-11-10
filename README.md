# pglord

Run highly-available PostgreSQL on Kubernetes from a web UI — **without installing
CRDs or holding cluster-admin**.

pglord is a control plane for HA PostgreSQL clusters: a Rust server with an embedded
Angular UI and a REST API. It provisions the instances, keeps one primary and one or
more streaming standbys, promotes a standby automatically when the primary fails, and
handles the day-2 work — backups, upgrades, pooling, TLS, roles — through the same
interface.

**Using pglord? Start with the [user guide](docs/user-guide.md)** — install, create a
cluster, connect an application.

## The bet

Every mature PostgreSQL operator is CRD-based, which means installing one takes
cluster-level privilege before you can create your first database. That is the right
trade for a platform team that owns the cluster. It is the wrong one if you have a
namespace and a ticket queue between you and the people who don't.

pglord makes the opposite choice by default:

- **No CRDs, no cluster-scoped anything.** A cluster's desired state is a `<name>-spec`
  ConfigMap. Every derived object — StatefulSet, Services, credentials Secret,
  per-instance RBAC — carries an ownerReference to it, so deleting a cluster
  garbage-collects everything. The server's ServiceAccount holds a plain namespaced
  Role. You can install pglord with the same rights you'd need to deploy any app.
- **The UI ships inside the server binary.** No second deployment, no separate image,
  nothing to wire up. (Zalando and StackGres also have good open-source UIs — this is a
  packaging difference, not a unique capability.)
- **Operator mode is there when you want it.** The same `ClusterSpec` type generates a
  `PostgresCluster` CRD, so you can move to `kubectl apply` and cluster-wide operation
  later without changing how anything behaves. You opt in; you don't start there.

pglord is young, and the cost of that choice is stated plainly in [Current
limitations](docs/user-guide.md#current-limitations).

## What it does

**Availability** — streaming replication, automatic failover on the highest-LSN ready
standby, planned switchover, optional synchronous replication with a strict quorum,
hibernation and per-instance fencing.

**Data safety** — pgBackRest backups to S3-compatible storage, scheduling with
retention, run history, point-in-time restore into a new cluster.

**Upgrades** — rolling minor upgrades, and major version upgrades in **two modes you
choose between**: blue-green via logical replication (applications keep running; writes
pause for seconds at cutover, and it needs a second full copy of the cluster) or
in-place `pg_upgrade --link` (minutes of downtime, almost no extra disk, gated on a
backup because a restore is the only way back).

**Configuration** — a curated GUC catalog that rejects settings HA depends on, `pg_hba`
rules, declarative roles, databases and extensions, one-time init scripts, custom
environment, placement/resources/anti-affinity, volume expansion.

**Access** — TLS from a `kubernetes.io/tls` Secret (cert-manager friendly), managed
PgBouncer, credential rotation, and API authentication via static tokens, OIDC or
Kubernetes tokens. OIDC and Kubernetes credentials can be authorized *per namespace*
by Kubernetes RBAC — pglord runs a SubjectAccessReview on every request, so whoever
may `kubectl patch` a cluster may do the same in the UI, and nobody needs a second
user database.

**Observability** — Prometheus metrics for clusters, instance readiness, failovers,
backup runs, rotations and synchronous degradation; an RBAC preflight page that asks the
API server what pglord is actually allowed to do.

## Compared with other PostgreSQL operators

These are all good projects, most of them older and more battle-tested than pglord.
The table is here to help you pick the right one, not to argue pglord wins.

| | CRDs required | Built-in web UI | HA mechanism | Backups | Major upgrades | License |
|---|---|---|---|---|---|---|
| **pglord** 0.2 | **No** (operator mode optional) | Yes, embedded in the server | Own controller | pgBackRest → S3 | Blue-green (online) **or** in-place `pg_upgrade` (downtime) | AGPL-3.0 |
| **CloudNativePG** 1.30 | Yes | None documented in the OSS project | Own controller | Barman (deprecating), plugins, volume snapshots | In-place `pg_upgrade` (downtime, since 1.26); logical-replication path also documented (online) | Apache-2.0, CNCF Sandbox |
| **Zalando** 2.0 | Yes | Yes, official and open source | Patroni (Spilo) | WAL-G | In-place `pg_upgrade` (downtime) | MIT operator, Apache-2.0 Spilo image |
| **Crunchy PGO** 6.0 | Yes | No — `kubectl pgo` CLI | Patroni | pgBackRest | `PGUpgrade` CR, `pg_upgrade` (full downtime) | Apache-2.0 code; **images restricted** — free production use only under 50 employees |
| **StackGres** 1.19 | Yes (14 of them) | Yes, official, on by default | Patroni | WAL-G | `SGDbOps`, `pg_upgrade` with copy/link/clone and a dry-run (downtime) | AGPL-3.0 (dual-licensed) |
| **Percona** 3.0 | Yes | No (OpenEverest is a separate product) | Patroni | pgBackRest | `PerconaPGUpgrade` CR (downtime) | Apache-2.0 |

*Verified against upstream docs in August 2026. These projects move quickly — check their
release notes before making a decision on this table alone. "None documented" means I
found no official UI in the project's docs or repo, not that one provably doesn't exist.*

**On the namespace question specifically**, since it is pglord's main claim: all five
require installing CRDs, which needs cluster-level privilege. After that, four of five
can watch a single namespace, with real differences — Crunchy ships a Role-only
`singleNamespace` install, Percona defaults to namespace scope, CloudNativePG still needs
a `ClusterRole` to read `nodes` and `ClusterImageCatalog`, and Zalando's chart always
creates a `ClusterRole`. pglord in standalone mode needs no CRDs and nothing
cluster-scoped at all. That is a genuine difference in kind, but "namespace-scoped
operator" is available elsewhere and may well be enough for you.

**A note on upgrade downtime:** every in-place `pg_upgrade` path above — including
pglord's — shuts the cluster down for the conversion. Only CloudNativePG and pglord
document an *online* path, and in both cases it is logical replication into a second
copy of the cluster, which costs double the disk and compute while it runs.

## Install

You need a Kubernetes namespace and `kubectl`. Images are published to GHCR by CI —
`pglord-server` and `pglord-agent` for PostgreSQL 16, 17 and 18, with and without
TimescaleDB — and the chart points at them by default, so nothing needs building first.

### With Helm (recommended)

```bash
helm install pglord ./charts/pglord \
  --namespace pglord --create-namespace \
  --set image.repository=ghcr.io/bigunmd/pglord-server
```

**`agentImage` chooses your PostgreSQL major.** It defaults to
`ghcr.io/bigunmd/pglord-agent:18`, so that install works as-is; set it to `:17`, `:16`
or a `-timescaledb` variant to run something else, or point it at your own registry.
Pick deliberately — changing it later is a major version upgrade. See
`charts/pglord/README.md` for every value.

### With plain manifests

```bash
kubectl apply -f deploy/k8s/           # namespace, namespaced RBAC, Deployment, Service
kubectl -n pglord rollout status deploy/pglord-server
kubectl -n pglord port-forward svc/pglord-server 8080:80   # http://localhost:8080
```

For shared access, expose the Service through your ingress rather than port-forwarding,
and turn on [authentication](docs/user-guide.md#11-authentication) — it is off until you
configure it, and until then anyone who can reach pglord can delete a cluster and its
volumes.

### Building the images yourself

```bash
make image agent-image                 # pglord-server:dev (UI embedded), pglord-agent:dev
make stand-up stand-deploy             # minikube: build, load and apply in one step
```

## Development

```bash
make help          # every target
make run           # server locally on :8080
make test          # cargo test --workspace
make lint          # fmt --check + clippy -D warnings
make openapi       # regenerate spec/openapi.yaml
make ui-dev        # Angular dev server
make stand-up stand-deploy stand-smoke
```

The API is code-first: axum + utoipa emit `spec/openapi.yaml`, and `ng-openapi-gen`
generates the TypeScript client. Both artifacts are committed and CI fails on drift —
after changing a handler or DTO, run `make openapi ui-client` and commit the result.

End-to-end suites run against a minikube stand (`make cluster-e2e failover-e2e day2-e2e
backup-e2e restore-e2e upgrade-e2e upgrade-inplace-e2e`, and more — see `make help`).
They exercise real clusters, real failovers and real upgrades rather than mocks.

Start with the [agent guide](AGENTS.md), [GOLD development process](docs/development.md)
and [current C4 architecture](docs/architecture/c4.md) for repository guidance,
verification expectations and the reasoning behind important boundaries.

## Configuration

Layered: serde defaults ← `config.toml` ← `PGLORD_*` environment variables.

| Variable | Default | Description |
|---|---|---|
| `PGLORD_HTTP_ADDR` | `0.0.0.0:8080` | HTTP listen address |
| `PGLORD_LOG_LEVEL` | `info` | tracing EnvFilter directive |
| `PGLORD_SHUTDOWN_TIMEOUT_SECS` | `25` | graceful drain budget (< k8s 30s grace) |
| `PGLORD_CORS_ALLOWED_ORIGINS` | *(empty)* | comma-separated exact origins; empty disables CORS |
| `PGLORD_AUTH_TOKENS` | *(empty)* | comma-separated static API tokens |
| `PGLORD_AUTH_OIDC_ISSUER` / `_CLIENT_ID` | *(empty)* | OIDC single sign-on |
| `PGLORD_AUTH_OIDC_RBAC` | `false` | authorize OIDC users through Kubernetes RBAC instead of full admin |
| `PGLORD_AUTH_KUBERNETES` | `false` | authenticate with Kubernetes tokens (TokenReview + SAR) |

Authentication is off unless one of the auth variables is set. See
[Authentication](docs/user-guide.md#11-authentication).

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).
