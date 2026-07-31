# Hardening + Failover + Clusters UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Three stacked slices: (H) replace trust auth with generated scram credentials; (F) automated failover — primary record with CAS, LSN-based election, promotion, `-rw` repoint, old-primary rejoin via pg_rewind; (U) clusters UI (list/create/delete) over the generated client.

**Architecture:**
- **H**: reconciler creates (create-only, never overwrites) a `<name>-credentials` Secret with random superuser + replication passwords; pods get them via `secretKeyRef` env; agent runs initdb with scram and idempotently ensures the `replicator` role after primary start; pg_hba narrows to scram.
- **F**: the current primary is recorded in annotation `pglord.dev/primary` on the spec ConfigMap. The server's orchestrator polls each instance's agent `/status` via headless DNS; on primary failure (3 consecutive misses) it elects the ready standby with the highest LSN and CAS-updates the annotation (resourceVersion optimistic concurrency). Everything downstream *follows the record*: the reconciler renders the `-rw` selector from it, the server POSTs `/promote` to the recorded primary, and each agent polls the record itself — a standby that sees itself recorded promotes; a primary that sees itself deposed self-fences (stops postgres, pg_rewind against the new primary, restarts as standby). Agents read the record through a per-cluster `pglord-instance` ServiceAccount+Role (get/list configmaps) rendered by the reconciler.
- **U**: `/clusters` route: table (name, image, replicas, ready), create dialog (name/replicas/image/storage), delete with confirm — all through the generated TS client, all strings via Transloco.

**Tech Stack:** rand (server), kube in agent (client only), pg_rewind; spartan table/dialog/input/label components.

## Global Constraints

- Secrets are **create-only** from the server: an existing `<name>-credentials` Secret is never modified (password rotation is out of scope).
- `wal_log_hints = on` in managed postgresql.conf (required by pg_rewind).
- The primary record annotation `pglord.dev/primary` holds a pod name (e.g. `demo-1`); absent = `<name>-0` (bootstrap default). All CAS writes go through one server-side helper.
- Agent K8s access is read-only ConfigMaps via the rendered `pglord-instance` SA; the agent must keep working with no K8s access (docker e2e) by falling back to ordinal/env behavior.
- Both e2e suites must stay green; a new `failover-e2e` proves: kill primary pod → record flips, standby promotes, `-rw` repoints, old primary rejoins as standby via pg_rewind.
- API/UI drift gates stay green (regen spec + TS client with any API change).

---

## Phase H: Credential hardening

### H1: Secret rendering + pod env
- `core/src/render.rs`: `pub fn credentials_secret_name(cluster) -> String` (`<name>-credentials`); StatefulSet env gains `PGLORD_AGENT_SUPERUSER_PASSWORD` / `PGLORD_AGENT_REPL_PASSWORD` via `secretKeyRef` (keys `superuser-password`, `replication-password`); unit tests assert the refs.
- `server/src/reconciler.rs`: `ensure_credentials(client, ns, spec, owner)` — `get_opt`; if absent, create Secret with two 32-char alphanumeric passwords (`rand::distr::Alphanumeric`), owner-ref'd to the spec CM. Called before apply_cluster.
- Tests: renderer refs; password generator length/charset.

### H2: Agent scram bootstrap
- `agent/src/config.rs`: `superuser_password` (default empty), `repl_user` (default `replicator`), `repl_password` (default empty).
- `bootstrap.rs`: initdb with `--auth=scram-sha-256 --pwfile=<tmp>` when password set (trust fallback when empty, keeping docker-compose-less local runs working); conf extra gains `wal_log_hints = on`; hba extra becomes `host all all 0.0.0.0/0 scram-sha-256` + `host replication <repl_user> 0.0.0.0/0 scram-sha-256` when password set (trust variant preserved when not).
- `pg_basebackup` gets `-U <repl_user>` + `PGPASSWORD` env.
- New `agent/src/postboot.rs`: after primary reports ready, idempotently `CREATE ROLE ... REPLICATION LOGIN` / `ALTER ROLE ... PASSWORD` for repl_user (SQL via local superuser connection, password from config).
- `status.rs` connection string gains `password=` when set.
- Update `scripts/agent-e2e.sh`: pass both passwords via `-e`, psql execs use `-e PGPASSWORD`.
- Verify: `make agent-e2e` green with scram.

## Phase F: Failover orchestration

### F1: Primary record helpers (server)
- `server/src/primary_record.rs`: `ANNOTATION = "pglord.dev/primary"`; `get_recorded_primary(cm) -> String` (annotation or `<name>-0`); `async fn cas_set_primary(api, cluster, expected_rv, new_primary) -> Result<bool>` — JSON merge patch guarded by `metadata.resourceVersion` precondition (send rv in patch body; apiserver 409s on mismatch → return Ok(false)).
- Reconciler: `-rw` selector rendered from the recorded primary → `render_rw_service(spec, ns, owner, primary_pod)`; renderer signature updated + tests.
- Render per-cluster `pglord-instance` SA + Role (get/list/watch configmaps) + RoleBinding, owner-ref'd; STS pods use the SA. Renderer tests.

### F2: Orchestrator loop (server)
- `server/src/orchestrator.rs`: per reconcile-cycle health check: for each instance pod `i in 0..replicas`, GET `http://<name>-{i}.<name>-pods.<ns>.svc.cluster.local:8008/status` (reqwest or raw hyper via kube client? use `reqwest` with 2s timeout). Track consecutive failures per pod in memory. When recorded primary fails 3 consecutive polls (poll interval 5s): candidates = ready standbys with `in_recovery=true`, parse `lsn` (`X/Y` hex → `(u64 X)<<32 | Y`), pick max; CAS the annotation; POST `/promote` to winner; log event.
- LSN parse unit tests (`0/3000000`, `1/ABCDEF00`, invalid).
- main: spawn orchestrator alongside reconciler.

### F3: Agent record-following (fence + rejoin + self-promote)
- `agent/src/record.rs`: optional kube client (in-cluster only; `kube::Client::try_default()` — on error, feature disabled). Poll spec CM every 5s: `recorded = annotation or <cluster>-0`; needs env `PGLORD_AGENT_CLUSTER` + `PGLORD_AGENT_NAMESPACE` (fieldRef) — rendered in H1/F1 sts env.
  - If I am recorded && in recovery → `pg_ctl promote` (self-promote; idempotent with server's POST).
  - If I am NOT recorded && NOT in recovery → **fence**: SIGINT postgres (via supervisor handle/pg_ctl stop -m fast), `pg_rewind --target-pgdata --source-server='host=<recorded fqdn> user=<superuser> password=...'`; on rewind failure → wipe PGDATA + pg_basebackup; write `standby.signal` + `primary_conninfo`; restart postgres. Supervisor needs a restart-capable loop: refactor `supervise` into a loop that the record-follower can ask to stop/start via a command channel (`tokio::sync::mpsc<SupervisorCmd>`; `Stop`, `Start`).
- Role resolution v2: `role=auto` consults the record when kube is available (recorded==me → primary, else standby with primary_host=recorded fqdn); ordinal fallback otherwise. Unit tests for decision function with record present/absent.
- Bootstrap standby `primary_host`: recorded primary fqdn (passed through resolved config), not hardcoded `-0`.

### F4: Failover e2e
- `scripts/failover-e2e.sh` + `make failover-e2e`: reuse cluster-e2e setup; then: write marker row → `kubectl delete pod demo-0` → poll until `demo-1` reports `in_recovery=false` (promoted) and spec CM annotation == `demo-1` (timeout 120s) → poll until recreated `demo-0` reports `in_recovery=true` (rejoined as standby) → insert row through `demo-rw` service (run a psql client pod or port-forward 5432) → assert row visible on `demo-0` (replicated through new primary). Delete cluster at end.
- Chaos acceptance: promotion < ~60s after pod kill; no write accepted by two nodes at once (rw selector follows record).

## Phase U: Clusters UI

### U1: Components + clusters page
- Vendor spartan `table`, `dialog`, `input`, `label` (CLI, tsconfig paths auto-updated).
- `ui/src/app/pages/clusters/clusters.component.ts`: signal-based list loaded via `Api.invoke(listClusters)`, auto-refresh every 5s; table columns name/image/replicas/ready; delete button per row with `confirm()`-style hlm dialog; create button opens dialog with form (name, replicas 1–5, image, storage) → `Api.invoke(createCluster, {body})`; problem+json `code` surfaced on error. All strings in `public/i18n/en.json` (`clusters.*`).
- Route `/clusters` + nav link in shell; home card links to it.
- `ng test` component smoke (renders with mocked Api), lint, build.

### U2: Live verification
- Rebuild + redeploy on stand; Playwright: open UI via port-forward, create cluster `ui-demo` (replicas 2) through the dialog, watch ready count reach 2, delete it. Screenshot for the record.

## Verification (end of plan)
1. `make lint test openapi-check ui-client-check` clean; `make ui-lint ui-build ui-test` clean.
2. `make agent-e2e` green (scram).
3. `make cluster-e2e` green (secret-based credentials in-cluster).
4. `make failover-e2e` green (kill → promote → repoint → rejoin).
5. Playwright: cluster lifecycle through the UI on the stand.
