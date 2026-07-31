# Cluster Detail Page Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A per-cluster detail view: spec summary, recorded primary, and a live per-instance table (pod, role, health, LSN) — the topology view that makes failover visible in the UI.

**Architecture:** The orchestrator's agent-polling moves to a shared `instance_probe` module; `GET /api/v1/clusters/{name}` grows into `ClusterDetail` (summary + `primary` + `instances[]`), probing agents over headless DNS exactly like the orchestrator does. UI adds `clusters/:name` with row-click navigation from the list, 5s auto-refresh, role badges and per-instance health dots reusing the established visual language.

## Global Constraints

- Instance observations come from one code path (`instance_probe`) — orchestrator and API must never drift.
- API change ⇒ regenerate `spec/openapi.yaml` + `ui/src/app/api` in the same commit (CI gates).
- All new UI strings via Transloco; both themes; no layout shift from async data (reserve space).

## Tasks

### D1: Shared instance probe + detail endpoint (server)
- New `server/src/instance_probe.rs`: `pub struct InstanceObservation { pub pod: String, pub ready: bool, pub in_recovery: Option<bool>, pub lsn: Option<String> }`, `pub async fn observe_cluster(http, cluster, ns, replicas) -> Vec<InstanceObservation>`; orchestrator refactored onto it (its election tuple building stays local).
- `SpecStore` gains `async fn primary(&self, name: &str) -> anyhow::Result<Option<String>>` (CM annotation, default `<name>-0` applied by caller); ConfigMap impl reads the annotation.
- `ClusterOps` gains `http: reqwest::Client` (2s timeout, built in main).
- `api/clusters.rs`: `ClusterDetail { name, image, replicas, ready_replicas, storage_size, primary, instances: Vec<InstanceDto> }`, `InstanceDto { pod, role, ready, lsn }` (`role`: `"primary"` if `in_recovery == Some(false)`, `"standby"` if `Some(true)`, else `"unknown"`); `get_cluster` returns it. List stays `ClusterSummary`.
- Existing no-kube 503 integration tests keep passing; unit test the role-mapping fn.

### D2: Contract regen
- `make openapi ui-client`; commit with D1: `feat(server): cluster detail endpoint with per-instance topology`.

### D3: UI detail page
- Route `clusters/:name` (lazy); list rows navigate on click (keep delete button click stopped from propagating).
- `pages/clusters/cluster-detail.component.ts`: back link, header `<name>` + primary badge, summary line (image, storage, replicas), instances table: pod (mono), role badge (primary = `bg-primary/10 text-primary`, standby = muted, unknown = warning), ready dot (success/border), LSN (mono, `—` when absent); 5s refresh; error surface as on list page.
- i18n `clusterDetail.*` keys; `make ui-build ui-lint ui-test` green; commit `feat(ui): cluster detail page with instance topology`.

### D4: Live verification
- `make stand-image` + redeploy; Playwright: create cluster, click row → detail shows demo-0 primary / demo-1 standby with LSNs; verify create-dialog no longer resizes on validation error; screenshots both themes; delete cluster; commit any fixes.

## Verification
`make lint test openapi-check ui-client-check ui-build ui-lint ui-test` all green; Playwright detail walkthrough on the stand.
