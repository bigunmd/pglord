# Synchronous Replication — Design

Date: 2026-08-02. Approved approach: a boolean spec flag; strict quorum semantics
(`ANY 1` over all peer pods); every agent renders its own static conf include; election
unchanged. Scope: spec + agent + observability + UI, day-2 togglable, reload-only.

## Context

pglord clusters replicate asynchronously: a primary crash can lose the commit tail, and
the user guide lists synchronous replication as a limitation. The v1 design doc planned
"sync as config flag if cheap" — this slice is that flag.

PostgreSQL makes a commit wait until standbys named in `synchronous_standby_names`
confirm the WAL flush. The pieces pglord already has line up well:

- Every standby's `primary_conninfo` carries `application_name=<pod name>`
  (`bootstrap.rs`), which is exactly the name `synchronous_standby_names` matches on.
- The agent already converges managed conf includes from the spec in its 5s watch loop
  (`apply_parameters` in `record.rs`) with content-compare-then-reload semantics.
- `synchronous_standby_names` is reload-safe (sighup), so the flag is a day-2 change
  with no restart and no roll.

Decisions folded in (settled during brainstorming, not configurable):

- **Boolean flag, quorum `ANY 1`.** `synchronous: true` means every commit waits for
  any one standby. No count knob, no FIRST/priority grammar, no explicit standby
  lists — pod membership is pglord-managed, so user-written lists could only break on
  scaling. A count knob can be layered on later without breaking old specs.
- **Strict: no auto-degrade.** When no standby is connected, writes block. That is the
  guarantee the user asked for by enabling the flag; silently dropping to async would
  void it exactly when it matters. pglord's job is to surface the state (API field,
  UI banner, metric), not to paper over it. An opt-in `preferred` mode can come later.
- **Election unchanged: max-LSN among reachable standbys.** Under `ANY 1` every
  committed transaction is flushed on at least one standby, so promoting the
  highest-LSN reachable standby loses nothing whenever the acking standby is
  reachable. Only a simultaneous primary+standby failure can lose the tail — the same
  exposure async has today, accepted rather than trading it for failover freezes.
- **Static peer list, rendered per instance.** Each instance's file names all pods of
  the cluster *except itself*. Listing a down standby is harmless: postgres only waits
  on connected ones, and none connected means blocked writes — which is the chosen
  strict semantics. Because every standby already carries the correct value for its
  own point of view, promotion needs no sync reconfiguration at all.

## Spec and validation (`pglord-core`)

`ClusterSpec` gains `#[serde(default)] pub synchronous: bool` — old spec ConfigMaps
parse as `false`. `SpecChange` gains `pub synchronous: Option<bool>` (absent =
untouched), making the flag reversible day-2.

Validation, in `ClusterSpec::validate` so every path including `apply_change` is
covered:

- `synchronous = true` requires `replicas >= 2`. A one-instance sync cluster has no
  standby to ack and would block writes forever; the same rule rejects scaling
  `replicas` down to 1 while the flag is on.
- `synchronous = true` rejects `parameters.synchronous_commit` values `off` and
  `local` — either silently defeats the guarantee. `on`, `remote_write` and
  `remote_apply` remain legal tuning.
- `synchronous_standby_names` joins `guc::RESERVED`: pglord owns it, user parameters
  cannot set it (it is not in the catalog today, so this only turns "unknown
  parameter" into the honest "reserved" rejection).

New module `crates/pglord-core/src/sync.rs` with one pure function:

```rust
pub fn render(cluster: &str, replicas: u8, me: &str, synchronous: bool) -> String
```

producing the entire content of the managed include file. Enabled, for instance
`demo-1` of a 3-replica cluster `demo`:

```
# Managed by pglord — do not edit; changes come from the cluster spec.
synchronous_standby_names = 'ANY 1 ("demo-0", "demo-2")'
```

Disabled: the same header plus `synchronous_standby_names = ''`, so turning the flag
off actually clears the value on reload rather than leaving it stale. Pod names are
double-quoted inside the single-quoted GUC value (they contain hyphens).

## Agent

A third managed include, `postgresql.pglord-sync.conf`, exactly the archive-conf
pattern in `params.rs`:

- `postgresql_conf_extra` in `bootstrap.rs` gains an `include_if_exists` line for it,
  alongside the existing params/archive includes and before the managed settings.
- Bootstrap writes the initial render; the watch loop (`apply_parameters` in
  `record.rs`) re-renders from the spec every pass and writes via a content-comparing
  `write_sync_if_changed`, OR-ing into the shared `changed` flag so the single
  `pg_ctl reload` at the end covers it. Toggling the flag or changing `replicas`
  converges on every instance within one 5s pass.
- The value is set on standbys too — postgres ignores it during recovery, and a
  promoted standby therefore has the correct value from its first read-write commit.

Bug fix folded in: `pg_basebackup -R` writes `primary_conninfo` *without*
`application_name`; today it is only corrected by `refresh_conninfo` on the next agent
restart. A freshly built standby would be invisible to `synchronous_standby_names`
until then. Fix: call `refresh_conninfo` immediately after `pg_basebackup` so the
standby connects correctly from its first start.

Existing clusters upgrade cleanly: the include file does not exist, the first watch
pass writes it (content `''` render when the flag is off) and reloads once.

## Server, contract and observability

No new endpoint — the flag rides the spec-update path; validation failures surface as
the usual RFC 9457 422 with distinct codes (`sync_requires_replicas`,
`sync_conflicts_synchronous_commit`).

Surfacing strict-mode state (the promise that makes strict acceptable):

- Agent `/status` gains `sync_standby_count: Option<u32>` — on a primary, the count of
  `pg_stat_replication` rows with `sync_state IN ('sync', 'quorum')`; `None` on
  standbys. Threaded through the single observation path: `InstanceStatus` (agent) →
  `AgentStatus` → `InstanceObservation` (`instance_probe.rs`) → `InstanceDto`.
- `ClusterDetail` gains `synchronous: bool` (echo of the spec) and `sync_degraded:
  bool` — true when the flag is on, the primary is reachable, and its
  `sync_standby_count` is 0: commits are blocking right now.
- Metrics: the agent exports the count as a gauge on a primary
  (`pglord_pg_sync_standbys`, following the agent's `pglord_pg_*` naming); the
  server exports `pglord_sync_degraded` per cluster.

After the DTO changes: `make openapi ui-client`, regenerated spec and client committed
with the code.

## Orchestrator

Unchanged. `elect_candidate` stays max-LSN; promotion needs no sync step (the standby's
include file already names its peers); the deposed primary's fence/rewind/rejoin path
is already correct for the same reason. A replicas change re-renders every instance's
file through the normal watch loop.

## UI

Cluster detail page:

- A "Synchronous replication" toggle (confirm-dialog pattern, like rotate-credentials)
  issuing `patchCluster({ synchronous })`. The dialog states the trade-off plainly:
  commits wait for a standby to confirm; if no standby is connected, writes block
  until one returns.
- A warning banner (restart-banner pattern) when `sync_degraded`: "Synchronous
  replication is on but no standby is connected — writes are blocked."
- Instance rows on a sync cluster show the primary's connected-sync-standby count.
- All strings through Transloco (`ui/public/i18n/en.json`).

## Testing

- **Core unit**: `render` byte-for-byte per ordinal (each instance excludes itself),
  enabled and disabled, 2 and 5 replicas; validation table — flag with `replicas = 1`
  rejected, `synchronous_commit = off` / `local` rejected with the flag on but accepted
  off, `synchronous_standby_names` in parameters rejected as reserved, old spec JSON
  without the field parses as `false`; `apply_change` rejects `replicas: 1` on a sync
  cluster.
- **Agent unit**: `write_sync_if_changed` idempotent (second pass writes nothing, no
  reload); bootstrap writes the include; post-basebackup conninfo carries
  `application_name`.
- **e2e** (`config-e2e.sh` section): on the stand cluster enable sync → primary's
  `SHOW synchronous_standby_names` returns the `ANY 1` list, `pg_stat_replication`
  shows `sync_state = quorum`, a test write commits, `restartCount` unchanged
  (reload-only), cluster detail reports `synchronous: true` and `sync_degraded:
  false`; 422 on `replicas: 1` patch while sync is on and on `synchronous_commit=off`;
  disable → `SHOW` returns empty. Failover behaviour is already covered by the
  existing failover e2e (promotion works identically).

## Docs

User guide: new "Synchronous replication" section next to the failover material —
what the flag guarantees, strict blocking semantics, the `replicas = 2` caveat (a
rolling restart of the only standby briefly blocks writes; three or more replicas keep
quorum through a roll), interaction with `synchronous_commit` tuning. Update the
failover section's "this is asynchronous" note and drop the "Asynchronous replication
only" limitation bullet.
