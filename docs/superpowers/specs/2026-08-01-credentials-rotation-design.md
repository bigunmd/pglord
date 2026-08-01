# Credentials Rotation — Design

Date: 2026-08-01. Approved approach: server-orchestrated rotation with a new agent
endpoint (approach A). Scope: pglord-managed credentials Secrets only; API + UI.

## Context

The `<name>-credentials` Secret is create-only: `ensure_credentials` never touches an
existing Secret, and the user guide lists password rotation as unsupported. The pieces a
rotation needs already exist separately: idempotent `ALTER ROLE` DDL in the agent's
`postboot.rs` (used at primary startup), a rolling-restart loop inside `restart_cluster`,
and peer auth on the agent's local socket (so DDL works even when the pod's env password
is stale).

One latent hazard makes naive rotation impossible today: a standby persists the
replication password inside `primary_conninfo` in `postgresql.auto.conf`, and an
already-bootstrapped standby's start path never rewrites it. After a password change the
standby would reconnect with the old password forever. This slice fixes that as part of
the design.

Decisions folded in (not configurable):
- Rotation always regenerates **both** passwords (superuser + replication) in one
  operation — one roll, one mental model.
- New passwords are always **server-generated** with the existing `generate_password()`
  (32-char alphanumeric, inside the agent's `is_safe_password` alphabet). No
  user-chosen values.
- User-supplied credentials Secrets are out of scope: pglord does not write to Secrets
  it does not own. The API refuses with a distinct error code.

## API

`POST /api/v1/clusters/{name}/rotate-credentials` → `202 Accepted`; the work runs in a
detached task, mirroring `restart_cluster`.

Pre-flight rejections (before the 202):
- unknown cluster → 404
- `spec.credentials_secret` set → 409, code `custom_credentials_secret`, detail
  explaining the user owns that Secret and rotation applies only to managed credentials
- no Kubernetes environment → 503 `kube_unavailable`

## Orchestration (detached task)

Four steps, each idempotent, so re-invoking rotate converges a partial failure:

1. **Generate** two new passwords via `reconciler::generate_password()`.
2. **Patch the Secret**: `stringData` merge patch on `<name>-credentials` updating both
   keys, plus annotation `pglord.dev/rotated-at: <RFC3339 UTC>`. The reconciler is
   create-only, so nothing reverts this write.
3. **ALTER on the primary**: `POST /roles/apply` on the recorded primary's agent with
   both passwords in the body. Existing replication streams stay up — postgres
   authenticates only at connect time.
4. **Roll pods**, standbys first, primary last, reusing the delete-and-wait loop
   extracted from `restart_cluster` into a shared `roll_pods` helper. Restarted
   standbys rewrite `primary_conninfo` from the new env at boot and reconnect with the
   new password against a `pg_authid` that already has it. The primary restart is an
   env refresh; postboot's `ensure_roles` re-applies the same DDL as a backstop.

Failures after the 202 are logged with cluster context and counted in a new metric
`pglord_credential_rotations_total{cluster, outcome="succeeded"|"failed"}`; the
endpoint stays safe to call again.

## Agent changes

- **`POST /roles/apply`** — primary only (409 `{"error": ...}` otherwise, same guard
  shape as `/backup`). Body `{"superuser_password": "...", "repl_password": "..."}`.
  Both values validated with `is_safe_password`; invalid input → 400. Applies the same
  `$pglord$`-quoted idempotent DDL as postboot, over the local socket (peer auth). The
  DDL helpers are extracted from `postboot::try_ensure` into shared functions rather
  than duplicated.
- **`primary_conninfo` refresh on every standby boot**: in `ensure_bootstrapped`'s
  already-bootstrapped path, when the role is standby and `standby.signal` exists,
  rewrite `postgresql.auto.conf` — strip every existing `primary_conninfo` line, append
  a fresh one from env. This closes the rotation hazard and normalizes duplicate lines
  accumulated by `pg_basebackup -R` plus `demote_to_standby`'s append.

## Contract and UI

This slice changes the API contract: `make openapi ui-client` regenerates
`spec/openapi.yaml` and the TS client, committed together with the code.

UI (`cluster-detail.component.ts`):
- New inline `RotateCredentialsDialogComponent` mirroring `RestartDialogComponent`
  (testid `confirm-rotate-credentials`).
- Launch button in the Secrets section, rendered only when the cluster has no
  user-supplied credentials Secret.
- Confirm copy warns: every instance restarts; connected clients keep working until
  they reconnect, then need the new password from the Secret.
- The `secrets.credentialsSecretFixed` i18n string (currently asserting rotation is
  impossible) is reworded to describe rotation.

## Docs

User guide: drop the rotation line from "Current limitations"; add a "Rotating
credentials" subsection near the Secrets material covering what rotates, the client
impact, how to read `pglord.dev/rotated-at`, and the trust-model note: passwords
transit unauthenticated in-cluster HTTP — the same channel `/promote` uses, and the
replication password already rests in plaintext in `postgresql.auto.conf`.

## Testing

- **Agent unit**: conninfo rewrite strips old lines / appends the fresh line / is
  idempotent; `/roles/apply` rejects on a standby (409) and on unsafe characters (400).
- **Server**: 409 guard for custom-secret clusters; new path present in the OpenAPI
  doc; pod-roll ordering extracted as a pure function and tested (standbys first,
  primary last).
- **e2e** (`secrets-e2e.sh`): rotate a running cluster, then assert psql succeeds with
  the new password, fails with the old one, and the standby still streams.
