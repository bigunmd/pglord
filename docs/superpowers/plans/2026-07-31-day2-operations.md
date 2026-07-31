# Day-2 Operations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn "delete and recreate" into real operations: planned switchover to a chosen instance, scaling instances up/down, and growing storage — all from the cluster detail page.

**Architecture:** Switchover reuses the failover machinery — it is the same primary-record CAS minus failure detection, so agents promote/fence exactly as they already do and `-rw` follows the record. Scaling and storage are spec edits: a `PATCH` endpoint validates the change, writes the spec ConfigMap, and the existing reconciler converges. Storage growth patches the PVCs directly (StatefulSet `volumeClaimTemplates` are immutable), then the reconciler leaves them alone.

**Tech Stack:** existing (kube-rs, axum/utoipa, Angular/spartan). No new dependencies.

## Global Constraints

- Shrinking storage is rejected (Kubernetes cannot shrink a PVC); growth requires a StorageClass with `allowVolumeExpansion` — surface the API error rather than pretending it worked.
- Scale-down deletes instances; the PVCs of removed ordinals are deleted too (StatefulSet leaves them behind) — this is destructive and must be confirmed in the UI.
- Switchover targets must be ready standbys; refuse otherwise with a problem+json code rather than flipping the record blindly.
- The primary record is only ever written through `cas_set_primary` — no new write path.
- API change ⇒ regenerate `spec/openapi.yaml` + `ui/src/app/api` in the same commit.

## Tasks

### O1: Spec-change validation in core
**Files:** `crates/pglord-core/src/spec.rs`

**Produces:** `pub struct SpecChange { pub replicas: Option<u8>, pub storage_size: Option<String> }`, `impl ClusterSpec { pub fn apply_change(&self, change: &SpecChange) -> Result<ClusterSpec, SpecError> }` — returns the new spec or an error; validates the result and rejects storage shrink with a new `SpecError::StorageShrink { from: String, to: String }`.
- Quantity comparison helper `fn quantity_bytes(s: &str) -> Option<u64>` (unit multipliers Ki/Mi/Gi/Ti) — unit-tested including `1024Mi == 1Gi`.
- Tests: replicas change validates bounds; storage growth ok; equal storage ok; shrink rejected (`2Gi -> 1Gi`, and `1Gi -> 1024Mi` is NOT a shrink); image/name are not changeable through `SpecChange`.
- Commit `feat(core): validated cluster spec changes`.

### O2: Switchover + patch endpoints
**Files:** `crates/pglord-server/src/api/clusters.rs`, `src/app.rs` (routes), `src/spec_store.rs` (`set_primary` via CAS), `src/reconciler.rs` (PVC expansion helper)

**Produces:**
- `POST /api/v1/clusters/{name}/switchover` body `{ "target": "demo-1" }` → 202. Validates: cluster exists (404); target is one of its pods (422 `invalid_target`); target is ready and `in_recovery == true` (409 `target_not_ready`); target is not already primary (409 `already_primary`). Then CAS the record and POST `/promote` to the target, same as the orchestrator's path.
- `PATCH /api/v1/clusters/{name}` body `SpecChange` → 200 `ClusterDetail`. Applies `apply_change`, stores the spec, and for a storage change patches every `<name>-data-<name>-<i>` PVC's `spec.resources.requests.storage`; PVC patch failure → 422 `storage_expansion_failed` with the apiserver reason (typical cause: StorageClass without expansion).
- `SpecStore::set_primary(&self, name: &str, target: &str) -> anyhow::Result<bool>` — read CM for resourceVersion, then `cas_set_primary`; false on lost CAS.
- Scale-down PVC cleanup: after a replica decrease, delete PVCs for removed ordinals (log failures, never fail the request — the spec change already applied).
- Tests: `pvc_name(cluster, ordinal)` helper; the no-kube 503 path for both new endpoints in `tests/http.rs`.
- Commit with regenerated contract: `feat(server): switchover, scale, and storage expansion`.

### O3: Detail page operations UI
**Files:** `ui/src/app/pages/clusters/cluster-detail.component.ts`, new `cluster-edit-dialog.component.ts`, `public/i18n/en.json`

- Header actions: **Edit** (opens dialog with instances segmented control 1–5 and storage input, prefilled; help text notes storage can only grow and scale-down removes data) and per-row **Make primary** button on ready standbys (hidden on the current primary and on unready rows), with a confirm dialog naming the target.
- Both call the generated client, refresh on success, and surface problem+json `detail` inline (the 409/422 codes above are all user-actionable).
- Loading/disabled states while a request is in flight so double-clicks cannot fire two switchovers.
- i18n `clusterDetail.edit*`, `clusterDetail.switchover*`.
- Commit `feat(ui): switchover and cluster editing from the detail page`.

### O4: Live verification + docs
- `scripts/day2-e2e.sh` + `make day2-e2e`: create 2-instance cluster → write a row → `POST /switchover {"target":"demo-1"}` → poll until record == `demo-1`, `demo-1` out of recovery, `-rw` endpoint == `demo-1`, row still readable → `PATCH {"replicas":3}` → poll until 3/3 ready and `demo-2` streaming → delete.
- Playwright on the stand: Make primary on a standby (primary marker moves), Edit → 3 instances (third row appears and turns ready).
- `docs/user-guide.md`: new "Day-2 operations" section — switchover before node maintenance, scaling, growing storage (and the StorageClass requirement), with the scale-down data warning.
- Commit `feat: day-2 operations e2e and user guide section`.

## Verification
1. `make lint test openapi-check ui-client-check ui-build ui-lint ui-test` green.
2. `make day2-e2e` green.
3. `make failover-e2e` still green (switchover must not have disturbed automatic failover).
4. Playwright: switchover and scale-up visible in the UI.
