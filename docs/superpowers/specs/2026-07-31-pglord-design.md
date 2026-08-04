# pglord — Kubernetes Postgres HA Management System: Design & Plan

## Context

pglord is a new open-source (AGPL-3.0) system for deploying, controlling, and monitoring HA
PostgreSQL clusters on Kubernetes. It is *not just a Helm chart*: it is a control plane with a
web UI, deployable either **standalone in a personal namespace** (no cluster-admin needed) or
later as a **cluster operator** (CRDs). The repo (`~/github/pglord`) is empty — greenfield.

Market research (CloudNativePG, StackGres, Zalando, Crunchy PGO, Percona) shows:
- A **web UI + REST API control plane is the biggest gap** in the ecosystem — only StackGres has
  one, and a UI is the most-requested missing feature in CloudNativePG's community.
- StackGres's key architectural win: the UI is an ordinary client of the public API; API auth
  maps onto Kubernetes RBAC; no privileged backdoor. pglord adopts this.
- pglord's combination — **own Rust instance manager (no Patroni) + pgBackRest + built-in UI +
  CRD-less standalone mode** — exists nowhere today.

## Decisions (settled with user)

| Decision | Choice |
|---|---|
| License | AGPL-3.0 |
| Language | **Rust** everywhere on the backend (low footprint) |
| HA engine | **Own instance manager** (CloudNativePG-style; K8s API as consensus store, no Patroni) |
| Deploy modes | **CRD-less standalone first** (namespaced Role, specs stored in-namespace); operator mode with CRDs later, same reconciler core |
| Backup engine | **pgBackRest** (MIT license — AGPL-compatible; orchestrated as external binary) — v1.1, not v1 |
| API | **Code-first spec-driven**: axum + utoipa → OpenAPI 3.1 artifact (CI-checked) → generated TS client |
| UI | **Angular** (standalone components) + **spartan** + **Tailwind**, served embedded by the server |
| V1 scope | **Core HA + UI basics** (see Milestones) |

## Feature matrix (from research — roadmap backbone)

**MUST HAVE (table stakes — every serious operator has these):**
HA + automated failover (streaming replication) · PITR backups to S3-compatible storage ·
restore/clone into new cluster · backup scheduling + retention · TLS (cert-manager friendly) ·
PgBouncer pooling · Prometheus metrics · rolling minor upgrades · postgresql.conf / pg_hba via
spec · user/database/role provisioning · resource sizing + PVC config + volume expansion ·
anti-affinity/topology spread · rw/ro Services · synchronous replication.

**SHOULD HAVE (strong differentiators most have):**
Major version upgrades (shipped as blue-green logical replication, not pg_upgrade — see
2026-08-04-major-upgrades-design.md) · standby clusters across K8s clusters (DR) ·
planned switchover · extensions management · declarative tablespaces · maintenance windows ·
WAL tuning · cluster hibernation + fencing (CNPG-only today, cheap to build, loved by users) ·
logical dump secondary backups.

**NICE TO HAVE (differentiation opportunities — few or none have):**
Web UI ✔ (pglord core bet) · REST API control plane ✔ · shipped Grafana dashboards ·
declarative logical replication · day-2 ops as first-class API objects (vacuum, repack, restart,
benchmark — StackGres-only today) · query/perf insights (pg_stat_statements in UI) ·
blue-green major upgrades · multi-tenant self-service DBaaS · cost/storage insights.

## Architecture

Cargo workspace, two deployable Rust binaries + one SPA:

```
UI (Angular+spartan) ──generated TS client──▶ pglord-server (axum REST + SSE)
                                                   │  reconciler (kube-rs)
                                                   ▼
                              K8s objects: StatefulSet, Services (-rw/-ro),
                              ConfigMaps (spec store + pg config), Secrets
                                                   │
                                                   ▼
                     Postgres pods: [init: copy agent] [pglord-agent (PID 1) → postgres]
                                                   │  local HTTP: health, LSN, exec ops
                                                   └────────── status ──▶ server ──SSE──▶ UI
```

### 1. `pglord-server` (crate `server`)
- Single Deployment in the user's namespace; namespaced Role only.
- Axum REST API, all handlers/DTOs utoipa-annotated; OpenAPI 3.1 emitted to
  `spec/openapi.json`, committed, CI job fails on drift.
- Serves the embedded Angular SPA (static assets via `rust-embed` or include_dir).
- **State**: desired cluster specs stored as labeled ConfigMaps in the namespace → server is
  stateless (no PVC), restart-safe. Storage is behind a `SpecStore` trait so operator mode later
  swaps in CRDs without touching the reconciler.
- **Reconciler**: level-triggered loop per cluster (kube-rs watchers + requeue). Renders and
  applies StatefulSet, `-rw`/`-ro` Services, ConfigMaps, Secrets (server-side apply, field
  manager `pglord`). Idempotent; status conditions per cluster; bounded in-memory event log
  surfaced in the UI timeline.
- **Auth (v1)**: token-based login against the server's own ServiceAccount-scoped model —
  minimal local admin auth; K8s-RBAC-mapped auth (StackGres-style) is a follow-up.

### 2. `pglord-agent` (crate `agent`)
- Instance manager, PID 1 in every Postgres pod. **Injected via init container** copying the
  static musl binary into a shared emptyDir → works with stock `postgres:NN` images.
- Responsibilities: initdb/pg_basebackup bootstrap (primary vs standby), start/stop/reload
  postgres, apply config (classifying GUCs reload-vs-restart), report health + replication LSN
  on a local HTTP endpoint, execute promote/demote on command, expose basic Prometheus metrics.

### 3. Failover model — server orchestrates, K8s is the consensus store
- The **current primary is recorded in one K8s object** (annotation on the spec ConfigMap or a
  Lease), updated only via resourceVersion CAS.
- On primary failure: server verifies via probes + agent status, picks the most-advanced ready
  replica by reported LSN, CAS-writes the new primary record, instructs promotion, repoints the
  `-rw` Service.
- **Fencing / split-brain**: an agent that observes it is *not* the recorded primary demotes
  itself and refuses writes. Single-writer is guaranteed by the record + CAS, not by network
  behavior. Manual fencing ("stop scheduling writes to instance X") is exposed as an API op.
- Planned **switchover** = same path minus failure detection (demote old → promote new).

### 4. UI (dir `ui/`)
- Angular standalone components, spartan/ui + Tailwind; TS client generated from
  `spec/openapi.json` (openapi-ts or orval) — no hand-written API calls.
- v1 screens: cluster list · create-cluster wizard (size, storage, replicas, PG version, config
  overrides) · cluster detail (topology: who is primary, replication lag, per-instance health,
  event timeline) · config editor (postgresql.conf params + pg_hba with reload/restart hinting) ·
  manual failover/switchover/restart actions. Live updates via SSE.

### 5. Shared crate (`core`)
- Cluster spec model (serde types, versioned `apiVersion`-style field for evolution — these types
  later become the CRD schema verbatim), GUC catalog (reload vs restart classification),
  validation, K8s object rendering helpers.

## Repo layout

```
pglord/
├─ Cargo.toml            # workspace
├─ crates/
│  ├─ core/              # spec model, validation, rendering, GUC catalog
│  ├─ server/            # axum API + reconciler + embedded UI
│  └─ agent/             # instance manager
├─ ui/                   # Angular + spartan + tailwind
├─ spec/openapi.json     # generated, committed, CI-checked
├─ charts/pglord/        # Helm chart: standalone deploy (Deployment+Role+RBAC)
├─ docs/
└─ e2e/                  # kind-based end-to-end tests
```

## Milestones

- **v1 (this plan): Core HA + UI basics, standalone mode.**
  Create/delete HA cluster (1 primary + N replicas, streaming replication, async; sync as
  config flag if cheap), automated failover + manual switchover, health/lag monitoring,
  remote config management (postgresql.conf + pg_hba, reload/restart aware), UI for all of the
  above, Helm chart to install pglord standalone.
- **v1.1: Backups** — pgBackRest sidecar/repo orchestration, scheduled backups, retention,
  PITR restore into new cluster.
- **v1.2: Operator mode** — CRDs generated from `core` spec types, controller frontend over the
  same reconciler; UI/REST auth mapped to K8s RBAC (StackGres pattern).
- **v1.3+:** minor rolling upgrades UX, PgBouncer, TLS via cert-manager, hibernation/fencing,
  then SHOULD-HAVE list (major upgrades, DR standby clusters, extensions…).

## Implementation order (v1)

1. **Workspace scaffold**: cargo workspace, `core` spec types + validation, CI (fmt, clippy, test).
2. **Agent MVP**: run postgres as child, bootstrap primary (initdb) & standby (pg_basebackup),
   health/LSN endpoint, promote on command. Testable locally in Docker without K8s.
3. **Reconciler MVP**: spec ConfigMap → StatefulSet/Services/Secrets rendering + server-side
   apply; cluster reaches Ready with primary + replicas streaming.
4. **Failover orchestration**: primary record + CAS, failure detection, promotion, `-rw`
   repoint, agent self-demotion fencing. Chaos test: kill primary pod → new primary < 30s,
   old primary rejoins as replica (pg_rewind if needed — decide during implementation).
5. **REST API layer**: utoipa-annotated endpoints (clusters CRUD, status, config, ops:
   switchover/restart/failover), SSE status stream, OpenAPI artifact + drift check.
6. **TS client gen + UI**: pipeline first (spec → client in CI), then the five v1 screens.
7. **Helm chart + docs**: standalone install, quickstart, demo script.

## Verification

- **Unit**: `core` rendering/validation; agent state machine with a faked postgres process;
  reconciler logic against kube-rs mock/recorded API.
- **E2E on kind** (`e2e/`): create 3-node cluster → assert streaming replication; kill primary
  pod → assert automated failover, no split-brain (old primary fenced), `-rw` repointed;
  change a reload-safe GUC → applied without restart; switchover via API → clean role swap.
- **API contract**: CI regenerates `spec/openapi.json` and fails on diff; TS client build +
  UI compile against it.
- **Manual demo**: `helm install pglord` on kind → open UI → create cluster → watch topology →
  `kubectl delete pod <primary>` → watch failover live in UI.
