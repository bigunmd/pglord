# Declarative role and database provisioning — design

Date: 2026-08-02
Status: approved

## Goal

Declare application roles and databases on the cluster spec and have pglord
converge them — on create, edit, failover and restore alike. Replaces the
role/database half of init scripts, which run once and never converge; init
scripts stay for arbitrary SQL. Closes the last MUST-HAVE item ("user/database/
role provisioning") on the design-spec feature matrix.

## Decisions (settled with user)

| Question | Decision |
|---|---|
| Passwords | Optional `password_secret {name, key}` reference per role (env `SecretKeyRefSpec` shape); no reference = role without a password. Never generated, never in the spec — the spec is a plain ConfigMap. |
| Removal | Leave in place. The spec is additive: pglord converges what is declared and never drops what isn't. No DROP anywhere in the reconcile path. |
| Surface | Curated minimal. Role: name, password_secret, login, createdb, createrole, connection_limit, in_roles. Database: name, owner. Grants/schemas/extensions are future slices. |

## Spec surface

Two lists on `ClusterSpec` (`crates/pglord-core/src/provision.rs`),
`#[serde(default)]`:

```jsonc
"roles": [
  {
    "name": "app_rw",
    "password_secret": { "name": "app-creds", "key": "password" },  // optional
    "login": true,               // default true
    "createdb": false,           // default false
    "createrole": false,         // default false
    "connection_limit": 20,      // optional; absent = unlimited
    "in_roles": ["reporting"]    // memberships, must name declared roles
  }
],
"databases": [
  { "name": "appdb", "owner": "app_rw" }   // owner must be a declared role
]
```

Validation, pure in `ClusterSpec::validate`:

- names match `[a-z_][a-z0-9_]*`, ≤ 63 bytes — safe to interpolate into DDL
  as quoted identifiers with no escaping surprises;
- reserved names rejected: `postgres`, `replicator` (the agent's fixed
  replication role), `pglord_pooler`, any `pg_*` prefix; for databases also
  `postgres`, `template0`, `template1`;
- no duplicate role or database names;
- every `in_roles` entry and every database `owner` names a declared role —
  ordering within the ensure is then trivial (roles first, then memberships,
  then databases);
- `password_secret` validated like the env secret refs (non-empty name/key).
- superuser/replication/bypassrls are not expressible — the struct simply has
  no such fields.

`SpecChange` gains both lists with the `pg_hba` semantics: absent = untouched,
present = full replacement, empty list = stop managing (nothing is dropped).

## Dependencies (deps.rs)

New `ResourcePurpose::RolePassword`. Every referenced Secret appears in
required-resources with its key (deduplicated and key-merged by the existing
machinery), so a missing or incomplete Secret is flagged in the UI before the
first connection fails.

## Agent endpoint `POST /provision/apply`

Same shape as `/pooler/ensure`: provably-primary guard (`in_recovery ==
Some(false)`), password charset check (`is_safe_password`) before any postgres
access, idempotent, one local superuser connection.

Body: the declared roles and databases, with password *values* substituted for
the references — the reconciler reads the Secrets and passes plaintext over
the pod network, exactly as rotation already does for `/roles/apply`.

DDL, in order:

1. **Roles** — for each declared role: `CREATE ROLE` if absent else `ALTER
   ROLE`, with `LOGIN`/`NOLOGIN`, `CREATEDB`/`NOCREATEDB`,
   `CREATEROLE`/`NOCREATEROLE`, `CONNECTION LIMIT n` (−1 when absent), and
   `PASSWORD $pglord$…$pglord$` when a password was resolved (re-applied every
   pass, same as the pooler role — cheap, and it is what makes Secret edits
   converge). No password reference → the password clause is simply omitted
   (an existing password is left untouched).
2. **Memberships** — grant each `in_roles` entry; revoke memberships *between
   declared roles* that the spec no longer lists. Convergence is
   attribute-level; only entity existence is leave-in-place.
3. **Databases** — `CREATE DATABASE "x" OWNER "y"` only when absent
   (existence-checked first; CREATE DATABASE cannot run in a transaction);
   `ALTER DATABASE "x" OWNER TO "y"` when the owner differs.

The SQL is built by pure, unit-tested functions; identifiers come from the
validated name charset and passwords from the dollar-quote-safe charset.

## Reconciler

The `apply_pooler` pattern verbatim, as `apply_provisioning`, called at the
end of `apply_cluster` when either list is non-empty:

- read each referenced Secret key; a missing Secret/key logs a warning and
  skips the pass (required-secrets is the user-facing surface for it) —
  postgres objects are untouched;
- POST `/provision/apply` to the recorded primary; refusal (standby, booting)
  or unreachable logs and retries next pass. Level-triggered, so it converges
  after create, edit, failover and restore with no new state.

Password rotation is: user updates their Secret, next pass re-ALTERs. No
pglord-side rotation flow for declared roles.

## API / UI

- Cluster-detail DTO gains `roles` and `databases` — the spec as configured,
  references only, never password values. `SpecChange` passthrough. New 422
  codes `invalid_role` and `invalid_database` (from `SpecError` variants
  wrapping the new `ProvisionError`).
- Contract as usual: `make openapi ui-client`, artifacts committed with code.
- UI: "Roles & databases" card on cluster detail with an editor dialog (env
  editor's row pattern — role rows: name, flags, secret name/key, memberships;
  database rows: name + owner select over declared roles), plus the same
  editor reachable from the create form's advanced section.

## Interplay

- **Pooler:** declared roles authenticate through `auth_query` with no extra
  configuration — the two features compose by construction.
- **Init scripts:** unchanged and still supported; docs position provisioning
  as the tool for roles/databases and init scripts for everything else
  (schema DDL, seeds, grants).
- **Restore:** `restored_spec` carries `roles` and `databases` over. The
  restored data already contains them; the first ensure is a no-op or fixes
  drift against the declared state.
- **Rotation:** the managed rotate-credentials flow is unrelated to declared
  roles (their passwords live in user Secrets).

## Error handling

| Failure | Behavior |
|---|---|
| Invalid name / reserved / dangling reference / duplicate | 422 problem+json, pure validation, stable codes |
| Referenced Secret or key missing | required-secrets flags it; reconciler logs and skips the pass |
| Password with unsafe charset in user Secret | agent answers 400; reconciler logs and retries (surface is the log + unconverged role) |
| Primary not ready | level-triggered retry next pass |
| CREATE DATABASE fails (e.g. template lock) | agent answers 500 with the postgres error; retried next pass |
| Role exists with different attributes (created out-of-band) | converged: ALTER brings declared attributes in line |

## Testing

- **Core:** validation matrix (names, reserved, duplicates, dangling refs,
  secret-ref shape); deps entries with/without references; SQL-builder unit
  tests (attribute rendering, membership grant/revoke sets, database
  create-vs-alter decisions).
- **Agent:** `/provision/apply` guard tests (charset 400, non-primary 409 —
  the `/pooler/ensure` shapes).
- **E2E** (`scripts/provision-e2e.sh`, `make provision-e2e`): declare role +
  database with a password Secret → psql as the role against the database via
  `-rw` and via the pool → flip a flag + membership, converges with no
  restarts → remove the entry, role and database still exist → update the
  password Secret, new password works → reference a missing Secret,
  required-secrets flags it → failover, ensure still converges.
- **Docs:** user-guide section; init-scripts section gains a pointer.

## Out of scope (deliberate)

Dropping roles/databases (`ensure: absent`) · object-level GRANTs · schema
management · per-database extensions · generated passwords · role password
rotation flows · quoting arbitrary identifiers (the name charset is the
contract).
