# Major version upgrades (blue-green logical replication) — design

Date: 2026-08-04
Status: approved

## Problem

`image` became day-2 mutable for minor upgrades (2026-08-02 design), but the
agent deliberately refuses to start when `PGDATA/PG_VERSION` disagrees with
the binary's major — so there is no major-upgrade path at all. Major upgrades
are the first SHOULD-HAVE roadmap item. A second, related gap: no pglord
image is published anywhere; `deploy/Dockerfile.agent` (postgres:17-based)
exists only for the dev stand, so users have nothing credible to upgrade *to*.

## Decisions (settled with user)

| Question | Decision |
|---|---|
| Downtime model | Blue-green via logical replication (not in-place pg_upgrade): parallel cluster on the new major, replicate, switch. |
| Identity | Same cluster identity, hidden green: the upgrade is an operation on the existing cluster; apps keep their Services and Secret. |
| Trigger | Manual gate: pglord automates up to `ready_to_cutover`; the user explicitly triggers cutover, and later finalize. |
| Rollback | Abort anytime pre-cutover; post-cutover rollback swaps Services back with an explicit "writes on the new version since cutover are abandoned" confirmation. No reverse replication. |
| Green model | Generation-suffixed StatefulSet under the one `<name>-spec` ConfigMap. No second cluster object, nothing renamed at finalize. |
| Images | Full GHCR matrix in this feature: agent images for PG 16/17/18, vanilla + timescaledb variants, plus the server image. |

## Generation model

`ClusterSpec` gains a persistent `generation` field (default `g1`; absent in
existing specs means `g1`). The render suffixes the StatefulSet name and a
generation label with it; cluster Services select
`cluster=<name>, generation=<gen>` (plus role labels as today). In steady
state exactly one generation exists. Pod/PVC names carry the generation
forever (`<name>-g2-0`); apps are unaffected because they use Services.

## Upgrade state

A new `upgrade` data key on the `<name>-spec` ConfigMap, updated with the
same resourceVersion-CAS discipline as the primary record:
`{ target_image, next_generation, phase, reason? }`.

Phases: `provisioning` → `replicating` → *(cutover)* → `soaking` →
*(finalize)* → key removed. `failed` (with reason) is reachable from
`provisioning`/`replicating`; `abort` is valid from `provisioning`,
`replicating`, and `failed`; `rollback` from `soaking`.

All state lives in the ConfigMap, so a server restart resumes monitoring
wherever the upgrade was. Day-2 spec edits (parameters, replicas, image,
workload, provisioning, restore) are rejected with 409 `upgrade_in_progress`
while the key exists. Failover on blue stays enabled (availability trumps
config), with the failure-model caveat below.

## Precondition: wal_level

Logical replication needs `wal_level=logical` on the publisher. The agent's
default `wal_level` changes to `logical` fleet-wide (CloudNativePG made the
same call; the WAL overhead is modest and it makes upgrades one-step).
Existing clusters converge on their next rolling restart. Starting an
upgrade checks `SHOW wal_level` on the primary and returns 409
`wal_level_not_logical` ("rolling restart required first") if it is still
`replica` — the existing pending-restart machinery and restart action cover
the remedy; no new flow.

## Green bootstrap (phase: provisioning)

While the upgrade key exists, the reconciler renders **two** StatefulSets
from the one spec: the current generation (current image) and
`<name>-<next_generation>` (target image, same replica count, same
credentials Secret, generation label, ownerReferences to the same spec
ConfigMap — delete-cluster GC still collects everything).

The green primary's agent starts in a new **subscriber mode** (env from the
render): fresh `initdb`, then against the blue rw Service (same superuser
password; pg_hba already admits cluster pods — green pods carry the same
cluster label):

1. apply declarative provisioning from the spec (roles, databases,
   extensions come from pglord, not from a dump);
2. `pg_dump --schema-only` per user database using the *new* binaries
   (new pg_dump reading an older server is the supported direction);
3. `CREATE PUBLICATION pglord_upgrade FOR ALL TABLES` per database on blue,
   and one subscription per database on green.

Green standbys are ordinary: the existing pg_basebackup bootstrap against
the green primary. Green has its own primary record annotation
(`pglord.dev/primary-<gen>`), managed by the same CAS helper, so green is
fully HA before cutover.

Direction guard: the versions are only truly known where postgres runs, so
the green agent compares its own major with the blue server's
(`SELECT version()`) during subscriber bootstrap and refuses when green's
major is below blue's; the failure surfaces as phase `failed`. The server
validates only non-empty target image + no upgrade already running.

## Replicating

The instance-probe path grows upgrade telemetry from green: per-database
`pg_subscription_rel` sync states and lag bytes (blue
`pg_current_wal_lsn()` minus the slot's `confirmed_flush_lsn`). When every
table is `r` and lag is under a small threshold, the API reports
`ready_to_cutover: true`.

Documented, not enforced: **no DDL between start and cutover** (logical
replication silently ignores it), and large objects are not replicated.
Sequences are handled at cutover.

## Cutover (user-triggered, requires ready_to_cutover)

Atomic-or-nothing sequence, ordered so a crash mid-way leaves either
"fenced, not swapped" (remedy: rollback un-fences) or "swapped" (soaking) —
never a half-swapped Service:

1. **Fence blue:** blue primary agent sets
   `default_transaction_read_only = on` (reload) and terminates open write
   transactions.
2. **Drain:** wait (bounded, ~60 s) until green's subscriptions confirm
   blue's current WAL LSN; on timeout, un-fence and report failure.
3. **Sequences:** copy every sequence's `last_value` blue → green
   (`setval` with a safety-margin bump), per database.
4. **Detach:** drop subscriptions on green, publications on blue.
5. **Swap:** patch Services' selectors to the green generation and promote
   the green primary record to be the cluster primary record.
   Phase → `soaking`. Blue stays up, fenced read-only.

## Soak, rollback, abort, finalize

- **Rollback** (from `soaking`; UI confirm carries the abandoned-writes
  copy): swap Services and primary record back, un-fence blue, tear down
  green STS/PVCs, clear the upgrade key.
- **Abort** (from `provisioning`/`replicating`/`failed`): drop
  publications on blue, tear down green resources, clear the key. Apps
  never left blue; trivially safe.
- **Finalize**: delete the blue STS and PVCs, persist
  `image = target_image` and `generation = next_generation` into the spec,
  clear the key. The green agent runs `pgbackrest stanza-upgrade` so
  backups work against the new major.

## Backups across the upgrade

Scheduled backups run against blue through `replicating`; suspended from
cutover to finalize (run history records skips with a reason); resumed
after finalize's stanza-upgrade. PITR across the upgrade boundary is
impossible by nature; the user guide says: take a final blue backup before
cutover (the UI suggests it), and restoring to before the upgrade means
restoring on an old-major image.

## Failure model

| Failure | Behavior |
|---|---|
| Blue failover during migration | Logical slots don't survive physical failover: subscriptions break, phase → `failed` with reason. Remedy: abort + restart the upgrade. Documented, not auto-healed. |
| Green pod/bootstrap failure | Level-triggered retries; persistent failure visible as any unhealthy instance; abort always available. |
| Cutover drain timeout | Un-fence blue, upgrade stays in `replicating`; cutover can be retried. |
| Day-2 edit during upgrade | 409 `upgrade_in_progress`. |
| Server restart mid-phase | State is in the ConfigMap; controller resumes. |
| Target image lacks a used extension | Green `CREATE EXTENSION` / preload fails during provisioning → visible failure, abort, pick a variant image that ships it (the matrix's timescaledb variants exist for exactly this). |

## API / UI / contract

- `POST /clusters/{name}/upgrade` (body: `target_image`),
  `POST .../upgrade/cutover|rollback|abort|finalize`.
- `ClusterDetail.upgrade` status block: phase, target image, per-database
  sync state, lag bytes, `ready_to_cutover`.
- Stable problem+json codes: `upgrade_in_progress`,
  `wal_level_not_logical`, `not_ready_for_cutover`, `no_upgrade_in_progress`,
  `invalid_upgrade_phase`.
- UI: Upgrade card on cluster detail — start dialog (target image), phase
  stepper with per-database progress and lag, then gated Cutover (danger
  confirm naming the write outage), Rollback (abandoned-writes confirm),
  Finalize (names blue deletion, suggests a final blue backup). Transloco
  strings; existing day-2 dialog patterns.
- `make openapi ui-client crd` after handler/DTO changes, committed
  together.

## Prebuilt image matrix (GH Actions)

New `.github/workflows/images.yaml` publishing to GHCR with
`docker/build-push-action` and buildx GHA cache:

|  | 16 | 17 | 18 |
|---|---|---|---|
| vanilla | `pglord-agent:16` | `pglord-agent:17` | `pglord-agent:18` |
| timescaledb | `pglord-agent:16-timescaledb` | `…:17-timescaledb` | `…:18-timescaledb` |

- `Dockerfile.agent` gains `ARG PG_MAJOR` (base `postgres:${PG_MAJOR}`) and
  an opt-in timescaledb layer (Timescale apt repo,
  `timescaledb-2-postgresql-${PG_MAJOR}`). Preload remains driven by the
  extensions feature.
- The server image (`deploy/Dockerfile`, embedded UI) is published as
  `pglord-server` — same distribution gap, same workflow.
- Tags: git tag `vX.Y.Z` → `vX.Y.Z-<matrix tag>` plus moving majors;
  pushes to main → `main-<matrix tag>`.
- User guide install section references these images instead of
  "build your own"; upgrade docs say: pick the same variant one (or more)
  majors up. Upgrades may skip majors (16 → 18) — a perk of logical
  replication.

## Testing

- **Core:** generation-suffixed render (STS name, labels, service
  selectors); upgrade-state serde and phase transitions as pure functions;
  sequence-sync SQL builders; the cutover sequence as a testable ordered
  plan.
- **Agent:** subscriber-mode decision table (fresh PGDATA + upgrade env →
  subscribe; existing PGDATA → normal path); direction guard; fence
  behavior.
- **Server:** 409 matrix for day-2-during-upgrade; CAS on the upgrade key;
  phase-gating of the four verbs.
- **E2e** (`scripts/upgrade-e2e.sh`, `make upgrade-e2e`; needs two agent
  majors cached in minikube): create on 16 with data + sequences + an
  extension → upgrade to 17 → green replicates, `ready_to_cutover` →
  write rows up to cutover → cutover → rows and sequence positions correct
  on green, Services point at g2 → finalize → blue gone, stanza works.
  Second scenario exercises rollback. Harness follows the e2e pitfalls
  notes (no assumptions about primary identity after rolls; verify image
  IDs).
- **CI images:** the matrix workflow builds on PRs touching the
  Dockerfiles (push only on main/tags).

## Implementation notes (deviations found while building)

1. **Globals come from the source, not only from the spec.** The design said
   green would provision from the declarative spec. That is not enough: a
   database created by hand outside pglord is owned by a role the spec does
   not mention, and its schema restore fails without that role. The green
   agent runs `pg_dumpall --globals-only` against blue first, then enumerates
   blue's databases and recreates each with its real owner.
2. **There is no separate service-swap step.** The `-rw` Service already
   selects the *recorded primary pod* by name, whatever generation it belongs
   to — so cutover is one CAS patch that moves the primary record and the
   phase together, and the Service follows. This removes the design's
   "swap Services" step entirely and makes the atomicity requirement trivially
   satisfied.
3. **Cross-generation record guard in the agent.** With one record shared by
   two instance sets, a green pod would read a record naming a blue pod and
   self-fence, and after cutover a blue pod would try to pg_rewind onto a
   different-major timeline. `record::same_instance_set` makes role actions
   apply only within a pod's own set; parameter convergence stays
   unconditional so green keeps picking up spec changes.
4. **Green archiving is disabled until finalize.** Archiving from green into
   blue's stanza fails on the system-identifier mismatch and retains WAL
   meanwhile. The agent suppresses archiving whenever `upgrade_source` is set;
   finalize's roll drops the env var, and its `stanza-upgrade` is what makes
   archiving correct again. Soaks should therefore be bounded.
5. **Sync-replication peers derive from the pod name, not the cluster name.**
   `synchronous_standby_names` listed `<cluster>-N`, which on green would name
   the wrong set's pods; it now derives the instance-set prefix from the pod's
   own name, falling back to the cluster name for non-pod hostnames.
6. **Scheduled and manual backups are skipped, not queued, during the soak**,
   with the reason logged and a 409 `upgrade_in_progress` on the manual path.
   Backups during the *migration* stay allowed on purpose — a final one before
   cutover is the only way back past the upgrade.
7. **Cutover is resumable.** If the data cutover succeeds but the record patch
   loses its CAS, the agent's `complete` marker lets a retried cutover skip
   straight to the record move instead of failing a drain against an
   already-detached source.
8. **No DDL against the source inside the fence window.** The cutover fences
   the source with `default_transaction_read_only`, and a read-only
   transaction refuses DDL — so `DROP PUBLICATION` cannot run there. Only the
   subscription is dropped (on the target, which is writable, and which is
   what removes the remote slot). The publication is inert afterwards and goes
   away with the rollback path's cleanup or with the source at finalize.
9. **The drain parameter binds through text.** `$1::pg_lsn` makes postgres
   infer `pg_lsn` for the parameter, which the client cannot serialize a
   string into ("error serializing parameter 0"); `$1::text::pg_lsn` keeps it
   a string. A unit test pins the cast.
10. **Green rendering stops once the spec adopts the generation.** Between
    finalize's spec write and its clearing of the upgrade state, a reconcile
    pass would otherwise re-render the promoted set as "green" and put the
    subscriber-mode env var back on the pods it is about to roll.

Items 8–10 were all found by running `make upgrade-e2e` against the dev stand;
none were reachable from unit tests.

## Out of scope (deliberate)

Reverse (green→old) replication · DDL capture/enforcement during
migration · large-object migration · automatic cutover · in-place
pg_upgrade mode · multi-version images · PG version matrix beyond 16–18 ·
non-timescaledb extension variants (addable to the matrix later).
