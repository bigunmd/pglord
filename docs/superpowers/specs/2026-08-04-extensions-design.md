# Declarative extensions management — design

Date: 2026-08-04
Status: approved

## Goal

Declare PostgreSQL extensions on the cluster spec and have pglord converge
them — both halves: `CREATE EXTENSION` in the target database, and
`shared_preload_libraries` for the extensions that need loading at server
start (pg_stat_statements, pg_cron, timescaledb — impossible to enable in
pglord today). First item of the post-v1.2 SHOULD-HAVE tier.

## Decisions (settled with user)

| Question | Decision |
|---|---|
| Scope | Both halves: per-database DDL and preload management via an explicit `preload` flag per entry. |
| Versions | Deferred. `CREATE EXTENSION` installs the image's default; no version field (addable compatibly later). |
| Removal | Leave-in-place, like roles/databases: entries removed from the spec stop being managed; nothing is dropped. The preload *list* is fully declarative (config, not data) and shrinks after the next restart. |

## Spec surface

`extensions: Vec<ExtensionSpec>` on `ClusterSpec` (`provision.rs`,
`#[serde(default)]`):

```jsonc
"extensions": [
  { "name": "pgcrypto", "database": "appdb" },
  { "name": "pg_stat_statements", "database": "postgres", "preload": true },
  { "name": "postgis", "database": "appdb", "schema": "gis" }
]
```

Validation, pure in `ClusterSpec::validate` (via the provision module):

- `name` and `schema` use the strict identifier charset roles use — the
  injection contract;
- `database` must name a declared database or `postgres`;
- no duplicate `(name, database)` pairs;
- reserved: `plpgsql` (built-in, managing it is noise).

`SpecChange` gains the list with the `roles` semantics: absent = untouched,
present = full replacement, empty = stop managing.

## Preload half

- `shared_preload_libraries` joins the RESERVED GUC list ("driven by the
  extensions block") so a hand-set value cannot fight the render.
- The agent's parameter-apply path renders
  `shared_preload_libraries = 'a,b'` from the **sorted, deduplicated** union
  of `preload: true` entry names (extension name = library name; that is the
  norm, and the spec keeps it that simple).
- Restart-required: the existing `pending_restart` machinery surfaces it and
  the existing rolling-restart applies it. No new restart flow.
- Empty union renders an empty setting (back to default) — removals converge.

Risk accepted and documented: a preload name whose library is missing from
the image makes postgres refuse to start on the next restart. The identifier
charset blocks injection, not typos; the user guide carries a warning and
the recovery (remove the entry, restart — the agent renders the shorter list
before postgres starts, because conf rendering happens agent-side pre-boot).

## DDL half

`/provision/apply` grows an `extensions` section, after roles → memberships
→ databases (so an extension can target a database declared in the same
edit):

- Pure SQL builders in core: existence check
  (`SELECT 1 FROM pg_extension WHERE extname = $1` shape) then
  `CREATE EXTENSION IF NOT EXISTS "name" [SCHEMA "schema"]`. Never altered
  after creation; schema changes to an existing extension are ignored
  (leave-in-place applies to attributes here — extensions are not ALTERed).
- The agent opens a short-lived unix-socket connection **per distinct target
  database** (today it connects only to `postgres`); failures per database
  are reported and do not abort the other databases.
- A preload-requiring extension whose library is not yet loaded fails with
  postgres's own error; level-triggered retries converge it after the
  restart. No special sequencing code.

## API / UI / contract

- `ClusterDetail.extensions` passthrough (references only — nothing secret
  here anyway). New `ProvisionError` variants surface as the existing 422
  `invalid_provision`.
- The "Roles & databases" card becomes "Provisioning" with an extensions
  table (name, database, schema, preload) and editor rows in the existing
  dialog; the pending-restart banner already explains the restart step.
- Contract + CRD regenerate from the shared types (`make openapi ui-client
  crd`).

## Error handling

| Failure | Behavior |
|---|---|
| Invalid name/schema charset, unknown database, duplicate pair | 422 `invalid_provision`, pure validation |
| Extension binary missing from the image | postgres error surfaced in agent response, logged by reconciler, retried each pass; user guide: bring an image that ships the extension |
| Preload library missing at restart | postgres fails to start; recovery documented (remove entry; agent renders the shorter list before postgres boots) |
| Preload-requiring extension created before restart | CREATE fails, retried next pass, succeeds after restart |
| Hibernated cluster | provisioning already skips |

## Testing

- **Core:** validation matrix (charset, unknown db, duplicates, plpgsql);
  SQL-builder tests; preload render tests (union, sort, dedup, empty,
  reserved GUC rejection of hand-set values).
- **Agent:** per-database exec shape and non-primary/charset guards
  (existing `/provision/apply` test patterns).
- **E2E** (`scripts/extensions-e2e.sh`, `make extensions-e2e`): declare
  `pgcrypto` in a declared database → function from it works there and not
  elsewhere; declare `pg_stat_statements` with preload → `pending_restart`
  true → restart → extension usable (`pg_stat_statements` view queryable);
  remove the pgcrypto entry → extension still present (leave-in-place);
  remove the preload entry + restart → `shared_preload_libraries` empty.
- **Docs:** user-guide section under provisioning; init-scripts pointer
  updated (extensions no longer need scripts).

## Out of scope (deliberate)

Version pinning/upgrades · `DROP EXTENSION` · ALTER of existing extensions ·
per-extension GUC bundles · extension allow-lists · library names differing
from extension names (rare; addable as an optional field later) · cascade
handling.
