# Prometheus Metrics — Design

Date: 2026-08-01. Approved approach: agent-native curated exporter (no sidecars).

## Context

pglord has no Prometheus metrics anywhere: the agent serves `/status` JSON on :8008, the
server serves `/healthz`/`/readyz` on :8080, and no metrics crate is in the workspace.
The design spec lists Prometheus metrics as table stakes and "expose basic Prometheus
metrics" as an agent responsibility. This slice adds curated metrics to both binaries.

Alternatives rejected:
- **postgres_exporter sidecar** — battle-tested but would be the first sidecar in the
  codebase (render/resources/image-pinning surface), a second superuser connection path,
  and hundreds of series when the goal is a curated set. Can still be added later.
- **Server-aggregated re-export** — single scrape point, but metrics die with the control
  plane, freshness is bounded by the 5s orchestrator poll, and per-instance stats are
  limited to what `/status` carries.

## Architecture

Two independent `GET /metrics` endpoints, Prometheus text format, both backed by the
`prometheus` crate (default features off):

- **Agent** (:8008) — route added to the existing axum router beside `/status`. Metrics
  are collected at scrape time using the same short-lived unix-socket superuser
  connection pattern as `status::try_query` (2s connect timeout, connection dropped
  after the pass). A new `crates/pglord-agent/src/metrics.rs` owns the SQL and the
  rendering; `status.rs` is untouched.
- **Server** (:8080) — route merged after the utoipa API router in `app::build()`,
  exactly like `/healthz`/`/readyz`: excluded from the OpenAPI doc and from CORS. No
  `spec/openapi.yaml` or generated-client changes.

No sidecars, no new container ports, no RBAC changes, no image changes.

## Agent metric set

All metrics prefixed `pglord_`. Instance identity (pod, cluster, namespace) comes from
Prometheus target labels supplied by discovery, not from metric labels.

| Metric | Type | Semantics |
|---|---|---|
| `pglord_pg_up` | gauge | 1 if the collection query succeeded, else 0 |
| `pglord_instance_primary` | gauge | 1 primary / 0 standby, from `pg_is_in_recovery()` |
| `pglord_pg_wal_lsn_bytes` | gauge | `pg_current_wal_lsn()` on primary, `pg_last_wal_replay_lsn()` on standby, as bytes |
| `pglord_pg_replication_lag_seconds` | gauge | standby only: `extract(epoch from now() - pg_last_xact_replay_timestamp())` |
| `pglord_pg_standby_replay_lag_bytes{standby}` | gauge | primary only: `pg_wal_lsn_diff(pg_current_wal_lsn(), replay_lsn)` per `pg_stat_replication` row, labeled by `application_name` |
| `pglord_pg_connections{state}` | gauge | `pg_stat_activity` counts grouped by state |
| `pglord_pg_max_connections` | gauge | from `pg_settings` |
| `pglord_pg_database_size_bytes{database}` | gauge | `pg_database_size()` for non-template databases |
| `pglord_pg_pending_restart` | gauge | any `pg_settings` row with `pending_restart` |

Collection is a single pass returning a plain `MetricsSample` struct; rendering the
sample into the registry is a pure function so unit tests need no postgres. `/metrics`
**always returns 200**: if collection fails, the response carries `pglord_pg_up 0` and
whatever process-local gauges exist. A scrape must never 5xx just because postgres is
down — that is exactly when the metric matters.

## Server metric set

A `Metrics` struct (one `prometheus::Registry` plus typed counter/gauge handles) is
constructed in `main()` and passed through `app::build()`'s state so integration tests
and `--dump-openapi` share the composition. The existing background loops record into it:

| Metric | Type | Source |
|---|---|---|
| `pglord_reconcile_total{cluster, outcome="ok"\|"error"}` | counter | `reconciler::run` per reconcile pass |
| `pglord_failover_total{cluster, trigger="auto"\|"manual"}` | counter | orchestrator election commits; switchover API |
| `pglord_backup_runs_total{cluster, kind, outcome}` | counter | backup run recording |
| `pglord_cluster_instances_ready{cluster}` / `pglord_cluster_instances{cluster}` | gauge | orchestrator's existing 5s `observe_cluster` pass (no extra polling) |
| `pglord_clusters` | gauge | count of spec ConfigMaps |

Per-cluster label series are removed when a cluster is deleted (registry cleanup on
spec deletion) so deleted clusters do not linger as stale series.

## Discovery

- **Postgres pods** — `render.rs` inserts default pod-template annotations
  `prometheus.io/scrape: "true"`, `prometheus.io/port: "8008"`,
  `prometheus.io/path: "/metrics"` *before* merging `spec.workload.annotations`, so a
  user-supplied `prometheus.io/*` key overrides the default (per-cluster opt-out).
  `prometheus.io/` remains an unreserved prefix.
- **Server pod** — the same annotations (port `8080`) in `deploy/k8s/deployment.yaml`
  and in the Helm chart's deployment template (kept in sync per the two-install-paths
  rule).
- **Helm chart** — new values, both default `false`:
  - `metrics.podMonitor.enabled` — renders a PodMonitor selecting
    `app.kubernetes.io/managed-by: pglord` pods on port name `agent`.
  - `metrics.serviceMonitor.enabled` — renders a ServiceMonitor for the pglord service's
    existing `http` port.
  Raw `deploy/k8s/` ships annotations only (monitors require prometheus-operator CRDs);
  documented in the user guide.

## Testing

- **Agent unit** — render a `MetricsSample` fixture into text and assert series names,
  values, and the `pglord_pg_up 0` degraded path.
- **Server integration** — `/metrics` serves the registry; `/api-docs/openapi.json`
  does not mention it; recording helpers bump the right series.
- **render.rs unit** — default scrape annotations present; user `prometheus.io/*`
  override wins; other workload annotations still pass through.
- **e2e** — agent-e2e (Docker) and the stand smoke gain a `curl /metrics` assertion on
  a known series; `helm template` exercised with the monitor toggles on and off.

## Docs

New "Monitoring" section in `docs/user-guide.md` with both metric tables and the
discovery story; remove the metrics gap from any limitations list it appears in.
