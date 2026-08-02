# PgBouncer pooling — design

Date: 2026-08-02
Status: approved

## Goal

Give every pglord cluster an optional managed connection pooler: a PgBouncer
deployment in front of the primary, reachable at `<name>-pool:5432`, so
connection-hungry apps stop exhausting `max_connections`. Closes the last
MUST-HAVE item on the design-spec feature matrix.

## Decisions (settled with user)

| Question | Decision |
|---|---|
| Topology | Separate Deployment `<name>-pooler` + Service `<name>-pool`, upstream `<name>-rw` — not a sidecar |
| Client auth | `auth_query` via a managed `pglord_pooler` role and SECURITY DEFINER lookup function; no userlist of app roles |
| Knob surface | Minimal curated knobs; everything else in pgbouncer.ini is pglord-managed and not exposed |
| TLS | Pooler follows cluster TLS automatically: terminates TLS for clients, `server_tls_sslmode=require` upstream |

## Spec surface

New feature block on `ClusterSpec` (`crates/pglord-core/src/pooler.rs`),
`#[serde(default)]` like backup:

```jsonc
"pooler": {
  "enabled": true,
  "replicas": 1,              // pooler pods, default 1, 1..=10
  "poolMode": "transaction",  // or "session"
  "maxClientConn": 100,       // 1..=100000
  "defaultPoolSize": 20,      // 1..=10000
  "image": "pgbouncer/pgbouncer:1.24.1",   // default, overridable
  "resources": { ... }        // same shape as workload resources
}
```

- `pool_mode = transaction` is the default; `session` for apps needing
  session state (prepared statements, advisory locks).
- Validation is pure in `ClusterSpec::validate`: bounds above, `replicas >= 1`,
  non-empty image. Knobs are accepted but inert while `enabled: false` (same as
  backup's schedule fields).
- Mutable via `SpecChange`, replace-wholesale semantics like `tls`: absent =
  untouched, block present = replace. Enable/disable/retune on a running
  cluster all supported; postgres pods are never rolled by pooler changes.

## Rendered objects (render.rs)

All ownerRef'd to the `<name>-spec` ConfigMap — deleting the cluster
garbage-collects the pooler like everything else.

- **Secret `<name>-pooler-config`** — two keys rendered from the spec. The
  render function stays pure: the reconciler reads `pooler-password` from the
  credentials Secret and passes it in as an argument.
  - `pgbouncer.ini`:

    ```ini
    [databases]
    * = host=<name>-rw port=5432 auth_user=pglord_pooler

    [pgbouncer]
    listen_addr = *
    listen_port = 6432
    auth_type = scram-sha-256
    auth_file = /etc/pgbouncer/userlist.txt
    auth_user = pglord_pooler
    auth_query = SELECT usename, passwd FROM pglord.pooler_lookup($1)
    auth_dbname = postgres
    pool_mode = <spec>
    max_client_conn = <spec>
    default_pool_size = <spec>
    ignore_startup_parameters = extra_float_digits,options
    ```

    `auth_dbname = postgres` pins the lookup to the `postgres` database, so the
    function exists in exactly one place regardless of which database a client
    targets. When cluster TLS is enabled the `[pgbouncer]` section additionally
    gets `client_tls_sslmode = allow`, `client_tls_cert_file` /
    `client_tls_key_file` (and `client_tls_ca_file` when `ca.crt` exists),
    plus `server_tls_sslmode = require`.
  - `userlist.txt` — exactly one entry: `"pglord_pooler" "<password>"`. The
    password comes from the credentials Secret (below); only the pooler's own
    credential ever lands in a file, never an app role's.
- **Deployment `<name>-pooler`** — `spec.pooler.replicas`, pgbouncer container
  running the spec image, config Secret mounted read-only at `/etc/pgbouncer`,
  TCP readiness probe on 6432, resources from the spec, standard pglord labels
  plus a role label distinguishing pooler pods from instances. A
  `pglord.dev/config-hash` pod-template annotation carries a hash of the
  rendered config, so any knob or password change rolls the pooler pods —
  pgbouncer never needs an in-place reload. When cluster TLS is enabled the
  cert Secret is mounted read-only at `/etc/pglord/tls` (non-optional volume,
  same reasoning as the instance mount).
- **Service `<name>-pool`** — port 5432 → targetPort 6432, selecting the pooler
  pods. Clients switch from `<name>-rw` to `<name>-pool` and change nothing
  else.

When `enabled: false` the reconciler **deletes** these three objects
(idempotent, NotFound is success). This is the first feature where reconcile
removes objects rather than only applying them.

## Auth provisioning

- The credentials Secret gains a third key, `pooler-password`, generated
  alongside the existing two when pglord owns the Secret. `deps.rs`: when the
  pooler is enabled and `credentials_secret` references a user Secret, the key
  is required and surfaced by the existing required-secrets flow.
- New agent endpoint `POST /pooler/ensure` (same provably-primary guard and
  password-safety checks as `/roles/apply`). Idempotently ensures, in the
  `postgres` database:
  - role `pglord_pooler` — `LOGIN` with the given password, no other privilege;
  - schema `pglord` (owned by the superuser);
  - `pglord.pooler_lookup(text) RETURNS TABLE (usename name, passwd text)` —
    SECURITY DEFINER over `pg_shadow`, `REVOKE ALL` from PUBLIC, `EXECUTE`
    granted only to `pglord_pooler`.
- The reconciler calls it level-triggered whenever the pooler is enabled and
  the primary probes ready — so it converges after create, enable, failover,
  and restore alike. No new orchestration state.
- Credentials rotation: the rotate flow also rotates `pooler-password` (ALTER
  ROLE via the same endpoint) and the pooler config Secret re-render + hash
  roll picks it up.
- pg_hba: no new managed rules. The pooler reaches postgres through the
  existing scram catch-all; on TLS clusters it connects with
  `server_tls_sslmode=require`, so `hostssl`-only / `hostnossl … reject`
  configurations keep working through the pool.
- Disabling the pooler leaves the role and function in place (harmless,
  documented); dropping them would race in-flight connections for no gain.

## Failover behavior

Upstream is `<name>-rw`; the orchestrator already flips its selector on
failover. In-flight pooled server connections break, pgbouncer re-resolves and
reconnects on the next client query — the same brief blip direct `-rw` clients
see. No pooler-specific failover logic.

## API / UI

- Cluster-detail DTO gains a `pooler` block: enabled, spec knobs, ready/desired
  pooler replicas (from Deployment status), pool service name. `SpecChange`
  gains the `pooler` block. New 422 error codes for bounds/image validation.
- Contract pipeline as usual: `make openapi ui-client`, commit regenerated
  artifacts together with the code.
- UI: pooler card on cluster detail — enable toggle, the four knobs, ready
  count, and the `<name>-pool:5432` connection hint. Pooler section on the
  create form, collapsed by default.

## RBAC / deploy paths

The reconciler manages a `apps/Deployment` for the first time: add
`deployments` (get/list/watch/create/patch/delete) to the namespaced Role in
**both** `deploy/k8s/` and `charts/pglord/` — the two install paths must stay
in sync by hand.

## Error handling

| Failure | Behavior |
|---|---|
| Spec knobs out of bounds | 422 problem+json with a stable `code`, pure validation |
| `pooler-password` missing from user Secret | required-secrets flags it; reconciler skips the pooler apply for the pass and logs/events the miss, postgres untouched |
| Primary not ready when ensuring auth | reconciler retries next pass (level-triggered); pooler pods start but clients fail auth until ensure lands |
| Pooler image unpullable | Deployment surfaces ImagePullBackOff; cluster detail shows ready 0/N; postgres unaffected |
| Cert rotation on TLS cluster | pgbouncer does not hot-reload certs — pooler pods must be restarted (`kubectl rollout restart`), documented limitation; postgres keeps its reload-without-restart behavior |

## Testing

- **Core:** pooler block validation matrix; ini render (knobs, TLS on/off,
  `auth_dbname`); userlist render; Deployment/Service/Secret render incl.
  config-hash annotation, owner refs, TLS mount; deps key set with/without
  pooler.
- **Agent:** `/pooler/ensure` idempotency, primary guard, password-safety
  rejection (existing `/roles/apply` test shapes).
- **Server:** spec-change accepts/rejects pooler block; reconciler
  deletes pooler objects on disable.
- **E2E** (`scripts/pooler-e2e.sh`, `make pooler-e2e`): enable pooler → psql
  via `<name>-pool` works → N concurrent clients share < N server connections
  (transaction pooling observable in `pg_stat_activity`) → an init-script role
  authenticates through the pool via auth_query → failover under pooled
  traffic recovers → knob change rolls only pooler pods → disable deletes the
  objects. TLS interplay asserted in `tls-e2e` (pool + `sslmode=require`).
- **Docs:** user-guide pooling section — when to use transaction vs session
  mode, the SAN note for TLS (`<name>-pool` must be in the cert), the
  cert-restart limitation, prepared-statement caveats in transaction mode.

## Out of scope (deliberate)

Read-only pooling in front of standbys (no `-ro` service exists yet) ·
pgbouncer metrics/`SHOW STATS` scraping · curated pgbouncer parameters catalog ·
per-database pool overrides · pooler PodDisruptionBudget · online config
reload (config changes roll pods instead).
