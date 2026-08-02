# Hot-standby GUC clamp — design

Date: 2026-08-02
Status: approved

## Problem

PostgreSQL refuses hot-standby recovery when certain GUCs on the standby are
set below the primary's **live** values: `max_connections`,
`max_worker_processes`, `max_wal_senders`, `max_prepared_transactions`,
`max_locks_per_transaction`. pglord renders the spec's parameter map verbatim
on every instance, so two real sequences brick standby rebuilds:

1. Raise `max_connections` in the spec, roll the cluster, then lower or clear
   it. The change is reload-only from pglord's point of view, so the primary
   keeps the higher value live (`pending_restart`). Any standby that now
   rebuilds via `pg_basebackup` renders the lower value and dies with
   `FATAL: recovery aborted because of insufficient parameter settings`,
   crashlooping until the primary restarts.
2. Restore/clone from a source cluster whose values are larger than the new
   cluster's spec.

Observed live on the dev stand while building the sync-replication e2e (see
`docs/superpowers/plans/2026-08-02-sync-replication.md` and the project memory
`standby-rebuild-fails-on-lowered-restart-guc`).

`max_wal_senders` is out of scope: `postgresql_conf_extra` pins it to 10 as a
managed setting after the user include on every instance, so it can never
diverge between primary and standby. That leaves four clampable GUCs.

## Decision

The agent clamps the four GUCs **on standbys** to at least the primary's live
values. The spec keeps recording the user's intent; the clamp is a local,
self-healing override that converges away once the primary actually restarts
at the lower value. (Alternative rejected: server-side validation refusing the
downgrade — it blocks legitimate edits, needs live-value plumbing into pure
validation, and cannot cover the restore-from-larger-cluster case.)

## Mechanism

**Source of truth for the primary's live values: the standby's own control
file.** `pg_controldata -D $PGDATA` reports all four (`max_connections
setting`, `max_worker_processes setting`, `max_prepared_xacts setting`,
`max_locks_per_xact setting`). A fresh basebackup carries the primary's
control file, and a running standby's control file tracks the primary via WAL
parameter-change records — so the values are correct exactly when they matter
and require no network call.

**A fourth managed include file**, `postgresql.pglord-standby.conf`, listed in
`postgresql_conf_extra` after `postgresql.pglord-sync.conf`. Last assignment
wins in postgresql.conf, so the clamp overrides the user parameter file. The
managed literals that follow the includes touch none of the four GUCs, so
nothing overrides the clamp.

**Rendering is pure, in `pglord-core`** (new `standby` module):

```
render(params: &BTreeMap<String, String>, live: &ControlFileValues) -> String
```

For each of the four GUCs: effective spec value = the spec's parameter if
present and parseable, else the PostgreSQL default (`max_connections` 100,
`max_worker_processes` 8, `max_prepared_transactions` 0,
`max_locks_per_transaction` 64). Emit `guc = live_value` only when the live
value exceeds the effective spec value. With nothing to clamp (or on a
primary) the file is a comment-only header — content-compare keeps the watch
loop from reloading.

**Agent wiring:**

- New `pg_controldata` runner + pure line parser in the agent (missing binary
  or unparseable output → no clamp, warn, proceed — behavior equals today's).
- `main.rs`: after `ensure_bootstrapped`, before `spawn_postgres`, when the
  resolved role is standby, run pg_controldata and write the clamp file
  (mirrors the sync-conf initial write). Primaries write the empty render.
- Watch loop (`record.rs::apply_parameters`): render and
  `write_standby_if_changed` each pass — clamp from current control-file
  values on standbys, empty render on the primary — sharing the existing
  changed→reload flow. A promoted standby thus clears its clamp on the next
  pass; the settings are restart-required so the leftover values stay live
  until its next restart, which is harmless (they are ≥ the spec's values).

## Not changing

- Spec validation, API surface, UI, OpenAPI contract: none of this is
  user-visible except that rebuilds stop crashlooping.
- `render_parameters` and the user parameter file stay verbatim-from-spec.

## Testing

- Unit: render (clamps only when live > spec-or-default; empty render;
  unparseable spec values fall back to defaults), control-file parser
  (real pg_controldata output fixture), include-order assertion in
  `postgresql_conf_extra` tests, write-idempotency test in `params.rs`.
- E2e (extends `scripts/config-e2e.sh`, after the existing restart sections):
  raise `max_connections` to 150 → rolling restart → clear parameters → force
  a standby rebuild (delete its PVC + pod) → the standby must return ready
  while the primary still runs 150. Then a rolling restart of the whole
  cluster must converge every instance back to the spec value.
