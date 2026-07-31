# Embed UI + pglord-agent MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** (A) Serve the built Angular SPA from the pglord-server binary (single-binary story). (B) Build the pglord-agent MVP: a Rust instance manager that bootstraps a Postgres primary or standby, supervises the postgres process, reports health/role/LSN over HTTP, and promotes on command — proven by a Docker e2e with real streaming replication and promotion.

**Architecture:** Embed via `rust-embed` behind a cargo feature `embed-ui` (on in Docker, off for plain `cargo run` so local builds don't require the UI dist). A single SPA-aware fallback: `/api/*` misses stay problem+json 404; everything else serves embedded assets with index.html fallback. Agent: tokio process supervisor (agent = PID 1, postgres as child), bootstrap via `initdb`/`pg_basebackup -R`, status via tokio-postgres over the unix socket, promote via `pg_ctl promote`. E2E runs two `pglord-agent:dev` containers (postgres:17 base) on a Docker network: primary + streaming standby, write→replicate→promote.

**Tech Stack:** rust-embed 8 + mime_guess; tokio::process; tokio-postgres (no TLS); postgres:17 official image for the agent runtime/e2e.

## Global Constraints

- Agent env prefix `PGLORD_AGENT_`, same figment layering as server config.
- Agent HTTP is an internal control endpoint (default `0.0.0.0:8008`) — NOT part of `spec/openapi.yaml`.
- MVP auth posture: pg_hba uses `trust` inside the isolated e2e network. Marked `SECURITY:` in code; hardening (scram + generated credentials) is the reconciler slice's job.
- Every commit keeps `make lint && make test` green.

---

### Task A1: Embed UI into pglord-server

**Files:** `crates/pglord-server/Cargo.toml` (feature `embed-ui`, deps rust-embed+mime_guess optional), `crates/pglord-server/src/ui_assets.rs` (new), `crates/pglord-server/src/app.rs` (SPA-aware fallback), `deploy/Dockerfile` (node build stage + `--features embed-ui`), `.dockerignore` (ui excludes + allow dist? no — dist built in-image), `Makefile` (image builds UI in Docker; no host coupling).

- [ ] Step 1: Cargo: `rust-embed = { version = "8", optional = true }`, `mime_guess = { version = "2", optional = true }`; `[features] embed-ui = ["dep:rust-embed", "dep:mime_guess"]`.
- [ ] Step 2: `ui_assets.rs`: `#[derive(rust_embed::RustEmbed)] #[folder = "../../ui/dist/pglord-ui/browser/"] struct Assets;` + `pub fn serve(path: &str) -> Response` returning the asset with mime type, falling back to `index.html` (SPA routes), 404 only if even index.html is missing. Entire module `#[cfg(feature = "embed-ui")]`; a sibling `#[cfg(not(feature = "embed-ui"))] serve()` returns plain 404 text.
- [ ] Step 3: `app.rs`: replace the api fallback with a router-wide `spa_fallback(uri: Uri)`: path starts with `/api` → `ApiError::not_found()`; else `ui_assets::serve(path)`.
- [ ] Step 4: integration test additions in `tests/http.rs`: `/api/v1/nope` still problem+json 404 (existing); non-api path without feature → 404 (build runs without the feature by default).
- [ ] Step 5: `deploy/Dockerfile`: prepend `FROM node:22-alpine AS ui-build; WORKDIR /app/ui; COPY ui/package*.json ./; RUN npm ci; COPY ui/ ./; RUN npm run build`; in builder stage `COPY --from=ui-build /app/ui/dist ui/dist` before `cargo build --release -p pglord-server --features embed-ui`. Add `ui/node_modules`, `ui/dist`, `ui/.angular` to `.dockerignore`.
- [ ] Step 6: Verify: `make image`, `docker run -p 18081:8080`, `curl /` → HTML with `<pg-root>`, `curl /api/v1/ping` → pong, `curl /clusters` (SPA route) → HTML. Commit `feat(server): embed Angular SPA behind embed-ui feature`.

### Task A2: Agent scaffold + config

**Files:** `crates/pglord-agent/{Cargo.toml,src/main.rs,src/lib.rs,src/config.rs}`, workspace `Cargo.toml` members.

- [ ] Config fields (all `PGLORD_AGENT_*`): `role` (`primary|standby`), `pgdata` (default `/var/lib/postgresql/data/pgdata`), `port` (5432), `http_addr` (`0.0.0.0:8008`), `socket_dir` (`/var/run/postgresql`), `primary_host` (empty), `primary_port` (5432), `superuser` (`postgres`).
- [ ] `Role` enum with serde lowercase + `FromStr`; figment Jail tests for defaults + env override + invalid role rejection.
- [ ] Commit `feat(agent): scaffold with figment config`.

### Task A3: Bootstrap module

**Files:** `crates/pglord-agent/src/bootstrap.rs`.

- [ ] Pure renderers (unit-tested): `postgresql_conf_extra(port) -> String` (listen_addresses='*', port, wal_level=replica, max_wal_senders=10, hot_standby=on) and `pg_hba_extra() -> String` (`SECURITY: trust for MVP e2e only` — local all trust, host all 0.0.0.0/0 trust, host replication 0.0.0.0/0 trust).
- [ ] `ensure_bootstrapped(cfg)`: pgdata has `PG_VERSION` → skip. Else primary: `initdb -D pgdata -A trust -U {superuser}` then append renderers to `postgresql.conf`/`pg_hba.conf`. Standby: retry loop (2s backoff, max ~60 tries) `pg_basebackup -h primary_host -p primary_port -U {superuser} -D pgdata -R -X stream`.
- [ ] Commit `feat(agent): primary/standby bootstrap`.

### Task A4: Postgres supervisor

**Files:** `crates/pglord-agent/src/supervisor.rs`, `src/main.rs`.

- [ ] Spawn `postgres -D pgdata` via `tokio::process::Command` (kill_on_drop). On SIGTERM/ctrl-c: send SIGTERM to child (fast shutdown), await exit, exit 0. If child exits on its own: log and exit with its code (K8s restart semantics).
- [ ] main: config → bootstrap → spawn supervisor + HTTP server (Task A5) → select on either finishing.
- [ ] Commit `feat(agent): postgres process supervisor with signal forwarding`.

### Task A5: Agent HTTP endpoint

**Files:** `crates/pglord-agent/src/http.rs`, `src/status.rs`.

- [ ] Deps: axum (default), tokio-postgres 0.7. Connect per-request via unix socket (`host=<socket_dir> user=<superuser> dbname=postgres`).
- [ ] `GET /healthz` → 200 always (agent alive). `GET /status` → `{ role: "primary"|"standby", ready: bool, in_recovery: bool|null, lsn: string|null }` — `pg_is_in_recovery()`, lsn = `pg_last_wal_receive_lsn()` in recovery else `pg_current_wal_lsn()`; connection failure → `ready: false`, nulls. `POST /promote` → run `pg_ctl promote -D pgdata`, 202 on success, 409 if not in recovery.
- [ ] Commit `feat(agent): status and promote HTTP endpoint`.

### Task A6: Agent image + e2e (replication + promotion)

**Files:** `deploy/Dockerfile.agent`, `scripts/agent-e2e.sh`, `Makefile` (`agent-image`, `agent-e2e`).

- [ ] `Dockerfile.agent`: cargo-chef build of `pglord-agent` (like server) but runtime `FROM postgres:17`, `COPY` agent binary, `USER postgres`, `ENTRYPOINT ["pglord-agent"]`.
- [ ] `scripts/agent-e2e.sh` (set -euo pipefail, trap cleanup): build image; docker network `pglord-e2e`; run `pg-e2e-primary` (ROLE=primary, `-p 18008:8008`); wait `/status` ready; run `pg-e2e-standby` (ROLE=standby, PRIMARY_HOST=pg-e2e-primary, `-p 18009:8008`); wait ready + in_recovery=true; `docker exec` psql on primary: `CREATE TABLE t(x int); INSERT ... 42;`; poll psql on standby until `SELECT x FROM t` = 42 (replication proven); `curl -X POST :18009/promote`; poll standby `/status` until `in_recovery=false` (promotion proven); write on promoted node succeeds.
- [ ] Run `make agent-e2e` green. Commit `feat(agent): docker e2e proving streaming replication and promotion`.

## Verification

1. `make lint && make test` — all crates.
2. `make image && docker run` → `/` serves the SPA (Postgres-blue UI), `/api/v1/version` JSON, SPA deep-link serves index.html.
3. `make agent-e2e` — full pipeline: bootstrap primary → standby via pg_basebackup → streaming replication of real rows → promote → writes on new primary.
