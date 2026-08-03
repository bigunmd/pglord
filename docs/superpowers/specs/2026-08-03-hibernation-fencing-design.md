# Cluster hibernation and instance fencing — design

Date: 2026-08-03
Status: approved

## Goal

Two day-2 operations that stop postgres on purpose without losing anything:

- **Hibernation** — scale a whole cluster to zero pods while keeping every PVC,
  the credentials Secret and the spec; wake it later with data intact. For dev
  stands, staging environments and cost control.
- **Fencing** — stop postgres on one chosen standby while the rest of the
  cluster keeps serving; the pod stays up so the disk can be inspected and the
  instance can rejoin later. For forensics and keeping a misbehaving replica
  down.

Closes the "cluster hibernation + fencing" item the design spec calls
"CNPG-only today, cheap to build, loved by users".

## Decisions (settled with user)

| Question | Decision |
|---|---|
| Scope | Both features in one slice — they share the agent-side "postgres stays down" mechanism. |
| Fencing the primary | Rejected with 409. Switchover first (already implemented), then fence the ex-primary. One API call does one thing. |
| Representation | Declarative spec fields converged by the existing reconciler/agent loops — not annotations, not direct StatefulSet surgery. |
| UI | Full: hibernate/wake with confirm dialog, distinct hibernated state on list + detail, per-instance fence/unfence. |

## Spec surface

Two fields on `ClusterSpec`, `#[serde(default)]` so existing specs parse
unchanged:

```jsonc
"hibernated": false,       // desired replicas is preserved; hibernation never edits it
"fenced": ["demo-1"]       // pod names, standbys only (enforced at the endpoint)
```

Validation, pure in `ClusterSpec::validate`:

- every `fenced` entry matches `<cluster>-<ordinal>` with ordinal <
  `replicas`; anything else is a new `SpecError` variant (422
  `invalid_fenced`);
- no duplicate entries.

"Not the recorded primary" is deliberately **not** a spec invariant — the
record is an annotation, not spec data. The fence endpoint enforces it at
call time, and the orchestrator never elects a fenced pod, so the record
cannot move onto one afterwards.

Neither field enters `SpecChange`. Like `restore`, these are operations, not
settings: they are flipped only by the dedicated endpoints, so a `PATCH` can
never hibernate a cluster as a side effect.

Hibernation and fencing compose: a cluster with fenced instances can
hibernate, and the fenced list survives; on wake those pods come up with
postgres stopped, still fenced.

## Rendering (render.rs)

- StatefulSet `replicas: 0` when `hibernated`, else `spec.replicas` as today.
- Pooler Deployment `replicas: 0` when `hibernated` (pgbouncer pods have
  nothing to connect to).
- PVC retention `whenScaled` flips from `Delete` to **`Retain`,
  unconditionally**. Today scale-to-zero would destroy every data volume
  (`render.rs:518`). Normal scale-down semantics do not change: the explicit
  `delete_orphan_pvcs` path in `patch_cluster` already deletes orphaned PVCs,
  and hibernation simply never calls it. `whenDeleted` stays `Delete` —
  deleting a cluster still garbage-collects storage.
- Neither field touches the pod template, so hibernate/wake/fence never
  triggers a spurious roll via `pod_template_changed()`.

## Agent — fenced mode

Today `fence()` (deposed primary) stops postgres and exits; the kubelet
restarts the pod and postgres comes back. Manual fencing needs the opposite
tail: postgres stays down, agent stays up. The two mechanisms stay distinct.

- **Enter:** the follow loop (already polls the spec ConfigMap every 5s) reads
  `fenced`. If my pod name appears: `pg_ctl stop -m fast`, remember fenced
  state, skip promote/fence/parameter logic while it holds. The supervisor
  treats the postgres child exiting while fenced as expected, not fatal.
- **Boot:** the fenced check runs **before** postgres starts — a rescheduled
  fenced pod comes up with postgres stopped, no flap.
- **Exit:** when my name disappears from the list, the agent process exits;
  the kubelet restarts the pod and the normal bootstrap path (demote,
  pg_rewind, rejoin as standby) runs. Reuses the existing well-tested boot
  logic instead of duplicating rejoin-in-place.
- **Status:** `/status` reports `role: "fenced"`, `ready: false`; `readyz`
  answers 503 so the pod drops out of the `-ro` Service automatically. No new
  agent HTTP endpoint — fencing is entirely spec-driven.

## Server API

Four endpoints, the switchover/restart 202 pattern (validate synchronously,
flip the spec field via the SpecStore, let the level-triggered reconciler
converge):

| Endpoint | Validation → problem+json code |
|---|---|
| `POST /clusters/{name}/hibernate` | 404 `not_found` · 409 `already_hibernated` |
| `POST /clusters/{name}/wake` | 404 · 409 `not_hibernated` |
| `POST /clusters/{name}/instances/{pod}/fence` | 404 · 422 `invalid_target` (not one of this cluster's pods) · 409 `cannot_fence_primary` (pod is the recorded primary) · 409 `already_fenced` · 409 `hibernated` |
| `POST /clusters/{name}/instances/{pod}/unfence` | 404 · 422 `invalid_target` · 409 `not_fenced` |

While hibernated, mutating operations answer 409 `hibernated`: `PATCH`,
switchover, restart, rotate-credentials, manual backup, fence. A hibernated
cluster accepts exactly wake and delete. (Unfence stays allowed — editing the
fenced list while asleep is harmless and converges on wake.)

Switchover to a fenced target already fails with the existing
`target_not_ready` — fenced pods are never ready.

## Gating in the control loops

- **Orchestrator:** for a hibernated cluster, stop after the metrics block —
  no failure counting, no election — and reset the failure counter so a stale
  count cannot fire a failover mid-wake. Fenced pods are excluded from
  candidate election and their unreadiness is never failure evidence.
- **Backup scheduler:** skips hibernated clusters; the existing catch-up
  semantics produce exactly one scheduled backup after wake.
- **Pooler ensure / provisioning:** early return when hibernated so the
  reconciler does not warn every 60s about an unreachable primary.

## Status, DTOs, metrics

- `ClusterSummary` and `ClusterDetail` gain `hibernated: bool`;
  `ClusterDetail` gains `fenced: string[]` (spec as declared — the pods may
  not exist while hibernated). `InstanceDto.role` can now be `"fenced"`,
  straight from the agent's status.
- Contract as usual: `make openapi ui-client`, artifacts committed with code.
- New gauge `pglord_cluster_hibernated{cluster}`; existing `instances*` gauges
  legitimately drop to zero while asleep.

## UI

- **Cluster list:** hibernated clusters show a "Hibernated" badge instead of
  the ready-dots row, so sleep does not read as an outage.
- **Cluster detail:** hibernate/wake action with an hlm confirm dialog
  (hibernate warns that all instances stop and connections drop); a banner
  while hibernated; disabled state on the mutating actions the server would
  409 anyway.
- **Instance rows:** fence/unfence action per standby row (hidden on the
  primary); `fenced` role badge styled distinctly from an unready instance.
- All strings through Transloco, both locales.

## Error handling

| Failure | Behavior |
|---|---|
| Fence the recorded primary | 409 `cannot_fence_primary`, spec untouched |
| Fence a pod name outside the cluster | 422 `invalid_target` |
| Any mutating op while hibernated | 409 `hibernated` |
| Record CAS conflict during fence validation | the endpoint reads the record once; the orchestrator cannot elect a fenced pod, so the race window closes itself |
| Server restarts mid-hibernate/wake | spec fields are the source of truth; the level-triggered reconciler converges on the next pass |
| Wake with a fenced instance | pod starts, agent sees itself fenced before starting postgres, stays down — declared state wins |

## Testing

- **Core:** validation matrix (bad pod names, out-of-range ordinal,
  duplicates); render tests — replicas 0 when hibernated, pooler 0, retention
  `Retain`/`Delete` split, pod template unchanged by both fields.
- **Agent:** decision-table unit tests for the fenced branch of the follow
  logic (fenced beats promote/fence; boot check ordering).
- **Server:** endpoint validation tests in the existing `http.rs` style —
  each error code above, plus the 409 `hibernated` guard on every mutating
  route.
- **E2E** (`scripts/hibernate-e2e.sh`, `make hibernate-e2e`): create 2-instance
  cluster → write a row → fence the standby → postgres stopped in that pod,
  role `fenced`, cluster still serves → unfence → rejoins and streams →
  hibernate → 0 pods, PVCs still present, `hibernated: true` in the API →
  mutating ops 409 → wake → all instances ready, row still readable, one
  catch-up backup at most.
- **Docs:** user-guide section (hibernate/wake, fencing, interplay with
  backups and failover).

## Out of scope (deliberate)

Scheduled/auto hibernation · hibernating via kubectl annotation ·
fencing the primary (switchover exists) · fence-with-timeout ·
rejoin-in-place without a pod restart · cross-cluster DR standbys.
