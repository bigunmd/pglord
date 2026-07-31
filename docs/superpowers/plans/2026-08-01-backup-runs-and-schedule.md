# Backup Runs and Schedules Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** See backups that are running, succeeded or failed — not just what ended up in the repository — and let pglord take them on a schedule, choosing the backup type.

**Architecture:** A backup *run* is recorded by the server (which already orchestrates the backup) in a per-cluster ConfigMap, so history survives a server restart and needs no new storage. The repository listing stays separate and authoritative for "what can be restored"; runs answer "what happened, and is something happening now". A scheduler loop evaluates each cluster's cron expression once a minute and triggers a run when due, skipping clusters that already have one in flight.

**Tech Stack:** existing, plus the `cron` crate for schedule parsing.

## Global Constraints

- Runs are advisory history, never the source of truth for restorability — the repository listing is.
- History is bounded (keep the most recent 20 per cluster) so a ConfigMap cannot grow without limit.
- A cluster may have at most one run in flight; a second request while one is running returns 409 rather than queueing.
- A run left `running` by a server restart is reported as `unknown` after a timeout rather than lying that it is still going.
- Schedules are cron expressions in UTC; an invalid expression is rejected at save time with the parser's message.

## Tasks

### R1: Backup types and run records (core + server)
- `core::backup`: `BackupKind { Full, Diff, Incr }` (serde lowercase, default Full); `BackupSpec` gains `schedule: Option<String>` and `schedule_kind: BackupKind`; validation rejects an unparseable cron and a schedule finer than every 15 minutes.
- `server::backup_runs`: `RunState { Running, Succeeded, Failed, Unknown }`, `BackupRun { id, kind, trigger: "manual"|"schedule", state, started, finished: Option<i64>, error: Option<String> }`; `record_start/record_finish` writing a labelled `<cluster>-backup-runs` ConfigMap (JSON array, newest first, capped at 20), `list_runs`, and `stale_running_to_unknown(runs, now, timeout)` — pure, unit-tested.
- Tests: cap keeps newest 20; a `running` record older than the timeout reads back as `unknown`; cron validation accepts `0 3 * * *` and rejects `not a cron` and `* * * * *` (too frequent).

### R2: Typed backups and run tracking through the API
- Agent `POST /backup` accepts `{"kind":"full|diff|incr"}` (default full) and passes it through to pgBackRest.
- Server `POST /clusters/{name}/backups` accepts the same body, refuses with 409 `backup_in_progress` when a run is in flight, records the run before spawning and updates it on completion or failure (capturing the agent's error text).
- `GET /clusters/{name}/backups` returns `runs` alongside `backups`, with stale `running` records mapped to `unknown`.
- Regenerate spec + TS client in the same commit.

### R3: Scheduler
- `server::backup_scheduler::run(client, ns)`: every 60s, for each cluster with `backup.enabled` and a schedule, compute the previous fire time; if a run has not happened since it and none is in flight, trigger one with `schedule_kind` and trigger `"schedule"`.
- Pure `is_due(schedule, last_run, now) -> bool` with tests: due when no run since the last fire; not due immediately after a run; not due when the schedule has not fired yet.
- Spawned from `main` beside the reconciler and orchestrator.

### R4: UI
- Repository dialog gains a schedule field (cron, with a plain-English hint and examples) and a backup-type selector for scheduled runs; invalid cron surfaces the server's message.
- "Back up now" becomes a small menu offering full / differential / incremental.
- A **Runs** table above the repository table: state badge (running with a spinner, succeeded, failed, unknown), type, trigger, started, duration, and the error on failures. Auto-refresh already runs every 5s, so an in-flight backup appears without a reload.
- i18n for all of it.

### R5: e2e + docs
- Extend `backup-e2e.sh`: take an incremental backup after the full one and assert the type; assert a run record appears with state `succeeded`; assert a second concurrent request returns 409; set a schedule and assert it is stored and reported; assert an invalid cron is refused with 422.
- User guide: scheduling, backup types (what differential and incremental mean and why the full one still matters), and reading the runs table.

## Verification
1. `make lint test openapi-check ui-client-check ui-build ui-lint ui-test` green.
2. `make backup-e2e` green.
3. `make failover-e2e` still green.
