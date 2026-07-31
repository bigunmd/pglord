# Remote Config Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Tune PostgreSQL settings per cluster from the UI, with pglord telling the user up front whether a change applies live or needs a restart — and giving them a safe rolling restart when it does.

**Architecture:** Parameters live in the cluster spec (the ConfigMap agents already read for the primary record). Each agent renders them into `postgresql.pglord.conf`, which the bootstrapped `postgresql.conf` includes via `include_if_exists`; on change the agent rewrites the file and issues `pg_ctl reload`. Settings that Postgres cannot change without a restart are reported back through `pg_settings.pending_restart` — authoritative, from Postgres itself, not our guess — and surfaced per instance in the API and UI. A rolling restart endpoint restarts standbys first and the primary last.

**Tech Stack:** existing. No new dependencies.

## Global Constraints

- pglord-managed settings (`listen_addresses`, `port`, `wal_level`, `max_wal_senders`, `hot_standby`, `wal_log_hints`) are **reserved**: rejected with a clear error, never overridable, because HA correctness depends on them.
- Only catalog-known settings are accepted. An unknown name is a typo that would stop Postgres from starting; refusing is kinder than a crashloop.
- The catalog's reload-vs-restart classification is advisory for the UI; `pending_restart` from Postgres is the source of truth for what is actually outstanding.
- pg_hba editing is explicitly **out of scope** for this slice (it can lock users out or open access; it deserves its own design). Note it as a limitation.
- API change ⇒ regenerate `spec/openapi.yaml` + `ui/src/app/api` in the same commit.

## Tasks

### C1: GUC catalog and spec parameters (core)
**Files:** `crates/pglord-core/src/guc.rs` (new), `src/spec.rs`, `src/lib.rs`

**Produces:**
```rust
pub enum ApplyMethod { Reload, Restart }
pub enum GucType { Integer, Real, Bool, Enum, Memory, Duration, StringLike }
pub struct Guc { pub name: &'static str, pub kind: GucType, pub apply: ApplyMethod,
                 pub category: &'static str, pub summary: &'static str, pub default: &'static str }
pub fn catalog() -> &'static [Guc];              // ~30 curated, commonly tuned settings
pub fn lookup(name: &str) -> Option<&'static Guc>;
pub fn is_reserved(name: &str) -> bool;          // pglord-managed settings
pub fn validate_value(guc: &Guc, value: &str) -> Result<(), String>;
pub fn render_parameters(params: &BTreeMap<String,String>) -> String;  // conf file body
```
Catalog covers at least: `max_connections`, `shared_buffers`, `work_mem`, `maintenance_work_mem`, `effective_cache_size`, `wal_buffers`, `max_wal_size`, `min_wal_size`, `checkpoint_timeout`, `checkpoint_completion_target`, `random_page_cost`, `effective_io_concurrency`, `default_statistics_target`, `synchronous_commit`, `statement_timeout`, `idle_in_transaction_session_timeout`, `lock_timeout`, `log_min_duration_statement`, `log_statement`, `log_connections`, `log_disconnections`, `autovacuum_vacuum_scale_factor`, `autovacuum_analyze_scale_factor`, `autovacuum_max_workers` (restart), `autovacuum_naptime`, `max_worker_processes` (restart), `max_parallel_workers` , `max_parallel_workers_per_gather`, `timezone`, `track_io_timing`.

`ClusterSpec` gains `#[serde(default)] pub parameters: BTreeMap<String, String>`; `validate()` rejects unknown names, reserved names, and bad values. `SpecChange` gains `parameters: Option<BTreeMap<String,String>>` (full replacement, not merge — simpler mental model and lets the UI remove a setting).

Tests: unknown name rejected; reserved name rejected with its own error; integer/memory/bool value validation (`128MB` ok, `128 megs` not; `on/off/true/false` ok); rendering is deterministic (sorted) and quotes string values; empty map renders empty; catalog has no duplicates and no reserved entries.

Commit `feat(core): GUC catalog and cluster parameters`.

### C2: Agent applies parameters
**Files:** `crates/pglord-agent/src/bootstrap.rs`, `src/params.rs` (new), `src/record.rs`, `src/status.rs`, `src/main.rs`

- Bootstrap appends `include_if_exists '<pgdata>/postgresql.pglord.conf'` to postgresql.conf (after the managed block, so user settings win over defaults but never over the managed ones — managed settings are re-appended *after* the include so they always take effect last).
- `params::write_if_changed(pgdata, rendered) -> io::Result<bool>` — writes only when content differs; returns whether it changed.
- `params::reload(pgdata)` — `pg_ctl reload -D <pgdata>`.
- The record-follow loop (already polling the spec ConfigMap every 5s) also reads `data.spec` parameters, renders via `render_parameters`, and on change writes + reloads. Log the applied settings.
- `status.rs`: add `pending_restart: bool` from `SELECT count(*) > 0 FROM pg_settings WHERE pending_restart`; include in `/status`.
- Tests: `write_if_changed` returns false on identical content, true on change (tempdir); include line is idempotent (bootstrap twice → one line).

Commit `feat(agent): apply cluster parameters with reload and pending-restart reporting`.

### C3: API — parameters and rolling restart
**Files:** `crates/pglord-server/src/api/clusters.rs`, `src/instance_probe.rs`, `src/app.rs`

- `InstanceObservation`/`InstanceDto` gain `pending_restart: bool`.
- `ClusterDetail` gains `parameters: BTreeMap<String,String>` and `pending_restart: bool` (any instance).
- `PATCH /clusters/{name}` accepts `parameters` (validated by `apply_change`); 422 `invalid_parameter` on rejection with the setting name in the detail.
- `GET /api/v1/gucs` → the catalog (name, category, type, apply, summary, default) so the UI can render an informed editor without hardcoding.
- `POST /clusters/{name}/restart` → 202: restart standbys first, then the primary, each by deleting the pod (the agent exits cleanly on SIGTERM; the StatefulSet recreates it and the new process picks up config). Wait for each to report ready before the next; run it as a spawned task so the request returns immediately, and log progress.
- Tests: catalog endpoint shape; no-kube 503 for restart.

Commit with regenerated contract: `feat(server): parameter management, GUC catalog endpoint, rolling restart`.

### C4: UI config editor
**Files:** `ui/src/app/pages/clusters/cluster-config.component.ts` (new), detail page wiring, i18n

- On the detail page: a **Configuration** section listing current parameters (name, value, "live" or "needs restart" badge), an **Edit configuration** button, and — when any instance reports `pending_restart` — a banner with a **Restart cluster** action (confirm dialog explaining standbys restart first, primary last, brief write interruption).
- Editor dialog: pick a setting from the catalog (grouped by category, with its summary and default as helper text), set a value, remove a setting; shows a per-row hint "applies immediately" / "requires restart" from the catalog before saving; save sends the full parameter map.
- Client-side validation mirrors the server (non-empty value); server errors surface inline.
- i18n `config.*`.

Commit `feat(ui): cluster configuration editor with restart awareness`.

### C5: e2e + docs
- `scripts/config-e2e.sh` + `make config-e2e`: create cluster → PATCH `{"parameters":{"work_mem":"8MB"}}` → poll until `SHOW work_mem` returns `8MB` on both instances **without** a restart (assert pod restartCount unchanged) → PATCH `{"parameters":{"work_mem":"8MB","max_connections":"200"}}` → assert `pending_restart` true in the API → `POST /restart` → poll until `SHOW max_connections` = 200 and pending_restart false → assert the cluster stayed available (a row written before is still readable) → delete.
- `docs/user-guide.md`: "Tuning PostgreSQL settings" section — how to change settings, what live-vs-restart means, the reserved settings and why, and that pg_hba editing is not available yet.

Commit `feat: config management e2e and user guide section`.

## Verification
1. `make lint test openapi-check ui-client-check ui-build ui-lint ui-test` green.
2. `make config-e2e` green.
3. `make failover-e2e` still green (parameters must not disturb HA).
4. Playwright: set `work_mem` live, then `max_connections` → restart banner → restart → banner clears.

---

# Next slice: workload customization (planned, not in this slice)

Raised while planning config management. Two of these are not conveniences —
they are gaps in what pglord currently promises:

- **Pod anti-affinity (HA correctness gap).** Nothing today stops every
  instance of a cluster landing on the same node, so a single node failure can
  take out the "highly available" cluster entirely. Default should be soft
  anti-affinity by hostname (`preferredDuringScheduling`), with an option to
  require it. The research matrix lists this as table stakes.
- **CPU/memory requests and limits (missing).** The Postgres containers
  currently declare no resources at all, so they are BestEffort: first to be
  evicted under node pressure, and unschedulable onto reserved capacity. Also
  table stakes per the research.
- **Custom labels and annotations** on pods and services — cost allocation,
  Prometheus scrape config, service-mesh injection, backup tooling.
- **StorageClass** per cluster (today it silently takes the default; users with
  a fast-SSD class cannot select it).
- **imagePullSecrets** — the user guide tells people to push images to their own
  registry, which fails on a private one without this.
- **Tolerations / nodeSelector** for dedicated database node pools, and
  **priorityClassName** for eviction protection.

Shape: extend `ClusterSpec` with a `workload` block covering these, render them
into the StatefulSet, expose them in the create/edit dialogs, and add an e2e
asserting anti-affinity and resources land on the pod spec.
