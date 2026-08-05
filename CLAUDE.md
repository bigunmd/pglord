# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What pglord is

Control plane for HA PostgreSQL clusters on Kubernetes: a Rust server with an embedded
Angular web UI and REST API. Runs standalone in a single namespace — no CRDs, no
cluster-admin. Cluster specs are stored as `<name>-spec` ConfigMaps; every derived object
(StatefulSet, Services, credentials Secret, instance RBAC) carries an ownerReference to
that ConfigMap, so deleting a cluster garbage-collects everything.

## Commands

All routine work goes through the Makefile (`make help` lists everything):

```bash
make build / make run            # cargo build / run pglord-server locally (:8080)
make test                        # cargo test --workspace
make lint                        # cargo fmt --all --check + clippy --all-targets -- -D warnings
make openapi                     # regenerate spec/openapi.yaml (--dump-openapi)
make openapi-check               # fail if committed spec drifts from code
make ui-dev / ui-build / ui-lint / ui-test   # cd ui && npm start/run build/run lint/test
make ui-client                   # regenerate TS client from spec/openapi.yaml (ng-openapi-gen)
make ui-client-check             # fail if committed client drifts
```

Single Rust test: `cargo test -p pglord-core test_name` (crates: `pglord-core`,
`pglord-server`, `pglord-agent`). Rust edition 2024; clippy warns on `unwrap_used`.

Dev stand (minikube, profile `pglord`): `make stand-up stand-deploy stand-smoke stand-ui`.
`make stand-image` scales the server to 0 before swapping the cached image — minikube
can't remove an image a running pod uses.

E2e suites are shell scripts against the stand: `make cluster-e2e failover-e2e day2-e2e
config-e2e workload-e2e backup-e2e restore-e2e secrets-e2e upgrade-e2e upgrade-inplace-e2e` (backup/restore
need the dev MinIO from `deploy/k8s/dev/minio.yaml`; `upgrade-e2e` and `upgrade-inplace-e2e` build agent
images for two PostgreSQL majors, and the in-place suite also needs MinIO). `make agent-e2e` is Docker-only (no minikube).

## The contract pipeline (never hand-edit generated artifacts)

API is code-first: axum + utoipa in `pglord-server` emit `spec/openapi.yaml`, and
`ng-openapi-gen` generates the TS client under `ui/src/app/api`. Both artifacts are
committed and CI fails on drift. After changing any API handler or DTO run
`make openapi ui-client` and commit the regenerated files together with the code.
Never edit `spec/openapi.yaml` or `ui/src/app/api/**` by hand.

`app::build()` in `crates/pglord-server/src/app.rs` is the single composition seam: it
builds the full router + OpenAPI doc and is called from `main()`, `--dump-openapi`, and
integration tests, so wiring cannot drift between them.

## Architecture

Three crates plus the UI:

- **`crates/pglord-core`** — shared pure domain logic, no I/O: `spec.rs` (`ClusterSpec`,
  validation, `apply_change` — name/image/credentials-secret deliberately immutable),
  `render.rs` (renders StatefulSet/Services/Secrets from a spec), `guc.rs` (curated GUC
  catalog; reserved HA-critical settings are rejected), `backup.rs` (pgBackRest config —
  credentials come from a referenced Secret, never the spec), `restore.rs` (PITR args),
  `workload.rs` (placement/resources; reserved label prefixes rejected), `env.rs`,
  `init.rs`, `deps.rs` (which external Secrets/ConfigMaps a spec requires).
- **`crates/pglord-server`** — control plane: `spec_store.rs` (`SpecStore` trait,
  ConfigMap-backed today, CRDs possible later), `reconciler.rs` (level-triggered kube-rs
  watcher, server-side apply, field manager `pglord`), `orchestrator.rs` +
  `primary_record.rs` (failover: the current primary is the `pglord.dev/primary`
  annotation on the spec ConfigMap, updated only via one resourceVersion-CAS helper;
  orchestrator elects the highest-LSN ready standby), `instance_probe.rs` (single shared
  code path for polling agent `/status` — used by both orchestrator and detail API),
  `backup_runs.rs`/`backup_scheduler.rs` (run history in a `<cluster>-backup-runs`
  ConfigMap; cron scheduler), `preflight.rs` (RBAC preflight via SelfSubjectAccessReview),
  `error.rs` (RFC 9457 problem+json; clients key on the stable `code` field),
  `ui_assets.rs` (Angular dist embedded via rust-embed behind the `embed-ui` cargo
  feature — plain `cargo run` works without a UI build; SPA fallback keeps `/api/*`
  misses as problem+json 404).
- **`crates/pglord-agent`** — per-instance manager, PID 1 in the postgres container:
  `supervisor.rs` (postgres as child process, SIGTERM forwarding), `bootstrap.rs`
  (initdb / pg_basebackup / pgBackRest restore), record-following role logic (a standby
  that sees itself recorded promotes; a deposed primary self-fences, pg_rewinds, rejoins
  as standby), `http.rs` (`/status`, `/promote` control endpoints), `backup.rs`
  (pgBackRest wrapper), `initdb.rs` (one-time init scripts, run only on first PGDATA
  creation).
- **`ui/`** — Angular standalone app. Components use spartan-ng "hlm" primitives vendored
  under `ui/src/app/ui/`. All user-visible strings go through Transloco (`ui/public/i18n/`).
  Theme is a `.dark` class persisted in `localStorage['pglord.settings']` with an
  anti-FOUC script in `index.html`. API access only through the generated client.

## Two install paths that must stay in sync

`deploy/k8s/` (raw manifests) and `charts/pglord/` (Helm chart) describe the same
deployment — RBAC rules, Deployment, Service. A change to one usually needs the same
change in the other; nothing automated checks this.

## Design docs

`docs/superpowers/specs/2026-07-31-pglord-design.md` is the design spec;
`docs/superpowers/plans/` holds per-feature implementation plans with the rationale
behind most non-obvious decisions (failover model, GUC catalog, backup gating, etc.).
`docs/user-guide.md` is the user-facing guide and is referenced by several plans —
keep it updated when behavior changes.
