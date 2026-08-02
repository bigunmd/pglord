# Minor version upgrades (mutable image) — design

Date: 2026-08-02
Status: approved

## Problem

`image` is immutable after cluster creation, so there is no upgrade path at
all — a table-stakes gap called out in the user guide's limitations and the
v1.3 roadmap ("minor rolling upgrades UX"). The rolling machinery this needs
already exists: `roll_order`/`roll_pods` (standbys first, primary last, wait
for ready between pods) serve the manual restart and secrets rotation, and the
orchestrator already handles a primary deposed by a roll.

## Decision

Make `image` a day-2 spec change and give pglord full ownership of the roll:

1. **`SpecChange.image: Option<String>`** — accepted by `apply_change` with
   non-empty validation (`SpecError::InvalidImage` when blank). Cluster name
   and `credentials_secret` stay immutable; the spec.rs doc comment is
   rewritten accordingly.
2. **StatefulSet `updateStrategy: OnDelete`.** The k8s controller never rolls
   pods on its own; pod-template changes take effect only when pglord (or the
   user) deletes a pod. This replaces today's implicit reliance on the default
   `RollingUpdate` (highest-ordinal-first, primary-blind) for env and workload
   changes.
3. **The patch handler owns the roll.** `patch_cluster` compares the rendered
   pod template (`render_statefulset(...).spec.template`) for the current and
   next spec. When it differs — image, env, or workload alike — it:
   - stores the new spec,
   - synchronously calls `reconciler::apply_cluster` so the StatefulSet
     carries the new template before any pod dies (server-side apply with the
     same field manager as the reconciler, so the level-triggered loop stays
     idempotent on top of it),
   - spawns the existing `roll_pods(roll_order(...))` background task.
   Template unchanged → no roll, exactly as today for parameters/replicas/
   storage/synchronous.

## Failure model

- **Mid-roll server restart:** remaining pods keep the old template until
  deleted. Recovery is the existing manual rolling-restart action (or any
  later template-changing spec change). Documented in the user guide.
- **Bad image (pull error, broken binary):** the first standby rolled fails
  ready; `roll_pods` waits its 120 s budget, logs, and moves on — same
  behavior the restart action has today. The primary is last in the order, so
  writes survive a bad image long enough to revert it (patch the image back,
  which triggers a new roll).
- **Major-version image by mistake:** the server cannot judge an arbitrary
  tag, so the agent guards: when PGDATA exists, compare `PGDATA/PG_VERSION`
  with the major of `postgres --version` before starting postgres; on
  mismatch exit with a fatal log naming both versions and the remedy (revert
  the image). Reverting the spec image rolls the cluster back.

## UI + contract

- Cluster detail gains an image edit (dialog pre-filled with the current
  image, confirm copy warning that every instance restarts, standbys first).
  Follows the existing day-2 dialog patterns; strings via Transloco.
- `make openapi ui-client` regenerates `spec/openapi.yaml` and the TS client.

## Testing

- Unit: `apply_change` accepts image / rejects empty; rendered template
  difference detection (image and env change it, parameters do not);
  `render_statefulset` sets `updateStrategy: OnDelete`; agent major-check
  (match passes, mismatch errors, missing PG_VERSION passes as fresh).
- E2e (extends `scripts/day2-e2e.sh`): retag the dev agent image inside
  minikube under a second name, patch the cluster image, assert the roll is
  standbys-first-primary-last, every pod converges to the new image, and the
  cluster ends healthy with an intact primary record. Existing env/workload
  e2e must still pass — they now roll via the server, the main regression
  risk of the OnDelete switch.
