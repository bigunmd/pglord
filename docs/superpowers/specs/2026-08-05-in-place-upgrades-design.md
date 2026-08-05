# In-place major upgrades (pg_upgrade) — design

Date: 2026-08-05
Status: approved

## Problem

Blue-green upgrades (2026-08-04) build a full second copy of the cluster:
a 1 TB cluster with two instances needs 4 TB and double the compute for the
duration. That cost is inherent to the approach, and for large clusters it is
the reason not to upgrade at all. `pg_upgrade --link` converts a data directory
in place using hard links, needing almost no extra space, at the price of a
write outage of minutes and no way back except a backup.

Neither mode dominates. This adds in-place as a **second mode the user
chooses**, alongside blue-green, on the same upgrade surface.

## Decisions (settled with user)

| Question | Decision |
|---|---|
| Where the old binaries come from | An **initContainer running the cluster's current image** copies `/usr/lib/postgresql/<old>` and `/usr/share/postgresql/<old>` into a shared `emptyDir`. No new images: the per-version matrix is unchanged and any pair of majors works. |
| Disk/rollback stance | `pg_upgrade --link`. Rollback is "restore the backup"; starting is **refused** unless backups are enabled and a completed backup exists. |
| Operation shape | One action. pglord upgrades the primary, then re-clones each standby, without further prompts. |
| API shape | One verb: `POST /upgrade` grows `mode`. `UpgradeState` carries it. In-place uses start + abort only. |

## Mode on the existing surface

```jsonc
POST /api/v1/namespaces/{ns}/clusters/{name}/upgrade
{ "target_image": "ghcr.io/…/pglord-agent:17", "mode": "in_place" }
```

`UpgradeMode { BlueGreen, InPlace }`, `#[serde(default)]` = `BlueGreen`, so
every existing caller and the committed contract keep working. `UpgradeState`
gains `mode`, and `UpgradePhase` gains `Upgrading` (in-place only).

Phase machines stay disjoint:

- `BlueGreen`: `Migrating → Soaking →` finalize | rollback (unchanged)
- `InPlace`: `Upgrading →` cleared on success, or by abort

`cutover`, `finalize` and `rollback` reject an in-place upgrade with the
existing 409 `invalid_upgrade_phase`.

**`abort` is always available, and is honest about what it can undo.** It
clears the upgrade state and reverts the spec's image to the pre-upgrade one.
What that achieves depends on how far the conversion got, and pglord does not
pretend to know:

- Nothing converted yet (unpullable image, failed `--check`, initContainer
  broken): the instances come back on the old image and the cluster is exactly
  as it was. This is the case abort exists for.
- PGDATA already converted: the old image refuses to start on it — the
  agent's existing major-mismatch guard fires with a message naming both
  versions. The cluster is down and the remedy is to restore the backup.

Making abort always available is deliberate. The alternative — refusing it
once the pods have rolled — leaves a stuck upgrade with no way to clear the
state, and day-2 operations are blocked while an upgrade is recorded, so the
cluster would be unmanageable. The abort confirmation states both outcomes.

## Preconditions (start)

Checked in this order, all before any state is written:

1. Not hibernated; no upgrade already in progress (existing checks).
2. `mode: in_place` **requires `backup.enabled`** → 409 `backups_required`
   with a detail naming the operation's irreversibility.
3. A completed backup must exist in the run history → 409 `backup_required`.
   The UI's confirm dialog offers "Take a backup now" and re-checks.
4. The target major must be **higher** than the running one. Unlike
   blue-green, the server can check this here: the primary reports its
   `server_version_num` through the existing probe, and the target major is
   parsed from the image tag when it looks like a version, otherwise the
   agent's own guard (below) is the authority.

`wal_level` is irrelevant to this mode; no restart precondition applies.

## Rendering the upgrade pods

While an in-place upgrade is recorded, `render_statefulset` (not a second
StatefulSet — there is only ever one instance set in this mode) is rendered
with:

- `image = target_image`;
- an **initContainer** `pglord-oldbin` running the *current* image, copying
  `/usr/lib/postgresql/<old>/.` and `/usr/share/postgresql/<old>/.` into two
  subpaths of an `emptyDir`;
- that `emptyDir` mounted into the main container **at the old tree's original
  absolute paths** — `/usr/lib/postgresql/<old>` (subPath `lib`) and
  `/usr/share/postgresql/<old>` (subPath `share`);
- `PGLORD_AGENT_UPGRADE_IN_PLACE=<old-major>` on the main container, which is
  what tells the agent an in-place upgrade is expected.

**The mount paths are load-bearing, not cosmetic.** Debian compiles the share
directory into the binaries as an absolute path, so a PG 16 `initdb` copied to
`/pgbin/...` looks for `/usr/share/postgresql/16` and fails with
`could not open version file`. Mounting the tree where it was originally
installed makes the old binaries work unmodified. Verified on the stand's
images before this design was finalized: with the tree mounted at its original
paths, `pg_upgrade --check` reports *"Clusters are compatible"*, and a real
`--link` run converted a 1000-row PG 16 cluster that PG 17.10 then started and
served, with `pgcrypto` still working.

The old major comes from the *spec's* current image only when the tag is
parseable; otherwise the server reads it from the primary's reported
`server_version_num`, which is authoritative. It is recorded in `UpgradeState`
(`from_major`) so the render is stable across reconciles.

The server patches the StatefulSet and then deletes the pods primary-last
(reusing `roll_order`/`roll_pods`), so standbys are gone before the primary is
converted and cannot stream from a half-upgraded timeline.

## Agent behavior

`bootstrap::ensure_bootstrapped`'s decision table gains one branch, ahead of
the existing major-version guard:

- **PGDATA exists, its major is below the binary's, and
  `PGLORD_AGENT_UPGRADE_IN_PLACE` is set:**
  - *primary*: `initdb` a fresh `$PGDATA.new`, run `pg_upgrade --check`, then
    the real `pg_upgrade --link` with
    `--old-bindir=/usr/lib/postgresql/<old>/bin` and
    `--new-bindir=/usr/lib/postgresql/<new>/bin`; on success remove the old
    directory and rename `$PGDATA.new` to `$PGDATA`, then re-apply pglord's
    managed `postgresql.conf` block and `pg_hba.conf` (the new directory is a
    fresh initdb and carries neither). A failed `--check` aborts before
    anything is modified.

    `$PGDATA.new` must sit on the **same volume** as `$PGDATA` — `--link`
    creates hard links, which cannot cross filesystems. `$PGDATA` is
    `/var/lib/postgresql/data/pgdata` inside the PVC mounted at
    `/var/lib/postgresql/data`, so a sibling path satisfies this. Removing the
    old directory afterwards is safe precisely because the files are hard
    links: the data survives under its new name (this is what pg_upgrade's own
    `delete_old_cluster.sh` does).
  - *standby*: wipe PGDATA and fall through to the existing
    `bootstrap_standby` path, which basebackups from the (now upgraded)
    primary. Standbys therefore need no pg_upgrade at all.
- The existing guard still fires when the env var is absent, so an accidental
  major-version image change is refused exactly as today.

`pg_upgrade` runs as the `postgres` user with the data directory it already
owns. Its output is captured and logged; a failure leaves the agent exiting
non-zero so the pod restarts and retries, with the original PGDATA untouched
because `--check` gates the destructive run.

After a successful upgrade the agent runs `vacuumdb --all --analyze-in-stages`
in the background (pg_upgrade leaves no optimizer statistics; without this the
cluster comes back correct but slow, which reads as a failed upgrade).

## Completion

The server watches for **every instance ready and reporting the target
major** — the probe already carries readiness; `server_version_num` is added
to the agent status alongside `wal_level` for this. On that condition it
persists `image = target_image` into the spec and clears the upgrade state.
`generation` is untouched: in-place keeps the same instance set and the same
object names.

Backups: `pgbackrest stanza-upgrade` is run on the primary as part of
completion, exactly as blue-green's finalize does.

## Failure model

| Failure | Behavior |
|---|---|
| `pg_upgrade --check` fails (incompatible extension, unsupported datatype) | Nothing is modified; the agent logs the check output and exits. The pod crashloops with the reason visible; abort restores the previous image. |
| `pg_upgrade` fails mid-run | PGDATA may be inconsistent — this is the case the backup gate exists for. Reported as a failed upgrade with the recovery being restore-from-backup. |
| initContainer cannot copy (old image unpullable) | Pod never starts; the upgrade stalls visibly and abort restores the previous image. |
| Old binaries incompatible with the new image's shared libraries | Surfaces as a `pg_upgrade --check` failure, i.e. the safe path. Proven on the stand as an early task, because the whole mode rests on it. |
| Standby re-clone fails | The existing basebackup retry loop applies; the cluster runs degraded and converges. |
| Server restarts mid-upgrade | State is in the ConfigMap; completion detection is level-triggered and simply resumes. |
| Abort after PGDATA was converted | Allowed; reverts the image and clears the state, after which the old image refuses the converted directory. The cluster is down and the remedy is restore-from-backup — stated in the confirmation before it runs. |

Automatic failover is disabled for the duration (`failover_enabled` already
keys off the upgrade state; it gains the in-place phase): the instances are
deliberately down and mid-conversion.

## API / UI / contract

- `UpgradeRequest.mode`, `UpgradeStatusDto.mode`, `UpgradePhase::Upgrading`,
  new codes `backups_required` and `backup_required`.
- The start dialog becomes a two-choice: **Blue-green** (no downtime, needs a
  second full copy of the cluster) vs **In place** (minutes of downtime, needs
  a backup, almost no extra disk) — each with its cost stated, since that is
  the entire basis for choosing. In-place shows the backup precondition and a
  "take one now" affordance when it is unmet.
- While in-place runs the card shows per-instance progress (which instance is
  converting, which are re-cloning) from the existing instance list; there are
  no cutover/finalize buttons, only Abort while it is still safe.
- `make openapi ui-client crd` regenerate.

## Testing

- **Core:** `UpgradeMode` serde default (an object without `mode` parses as
  blue-green — the compatibility guarantee); render of the initContainer,
  emptyDir and env var; the old-major parse from an image tag, including
  unparseable tags falling back.
- **Agent:** the bootstrap decision table gains cases (older PGDATA + env set +
  primary → upgrade path; + standby → wipe and clone; env absent → the
  existing refusal); `pg_upgrade` argument construction as a pure builder.
- **Server:** the precondition matrix (no backups → `backups_required`; no
  completed backup → `backup_required`; lower target major → refused);
  cutover/finalize/rollback rejecting an in-place upgrade; completion
  detection as a pure function over instance observations.
- **E2E** (`scripts/upgrade-inplace-e2e.sh`, `make upgrade-inplace-e2e`):
  create on 16 with data, a sequence and pgcrypto; enable backups against the
  dev MinIO and take one; upgrade in place to 17; assert the cluster returns
  2/2 on PG 17, every row and the sequence position survived, pgcrypto still
  works, object names are unchanged (`updemo-0`, not `updemo-g2-0`), and the
  volume did not grow by the size of the data. Also assert start is refused
  without a backup.
- **Spike: done, before this spec was finalized.** A PG 16 tree copied out of
  the 16 image and mounted at its original absolute paths inside the 17 image
  passes `pg_upgrade --check` ("Clusters are compatible") and completes a real
  `--link` upgrade: 1000 rows preserved, PG 17.10 serving from the converted
  directory, `pgcrypto` intact. This is what the mode rested on, so it was
  proven before anything was built. It also produced the mount-path correction
  above.

## Out of scope (deliberate)

`--clone`/copy modes · the rsync standby-upgrade trick (fragile; re-cloning is
simpler and the disk cost is transient) · downgrades · extension binary
compatibility checking beyond what `pg_upgrade --check` does · upgrading a
hibernated cluster · in-place across more than one major in a single step is
allowed but untested beyond 16→18 in the suite.
