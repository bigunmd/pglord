# pg_hba.conf Editing — Design

Date: 2026-08-01. Approved approach: structured rules in the spec, fixed managed
prefix/suffix, agent owns the whole rendered file. Scope: API + UI, all instances of a
cluster share one rule list.

## Context

Today `pg_hba.conf` is initdb's generated file plus a block appended once at bootstrap
(`pg_hba_extra` in `crates/pglord-agent/src/bootstrap.rs`): a scram catch-all for all
network access and a replication rule (trust variants when no password is configured).
Nothing can change it after bootstrap; the user guide lists pg_hba editing as a
limitation, and the config-management slice deferred it because a wrong rule can lock
users out or open access.

The machinery this feature needs already exists for `postgresql.conf`: spec-driven
parameters validated in `pglord-core`, applied by the agent's spec-watch loop
(`apply_parameters` in `record.rs`) with content-compare-then-reload semantics. pg_hba
follows the same shape. Unlike GUCs there is no restart class: pg_hba changes are
always reload-safe.

Decisions folded in (not configurable):

- **Structured rules, not raw lines.** Each rule is an object the core can validate
  field by field; no raw pg_hba text ever passes through the API.
- **Fixed prefix + suffix.** pglord's managed rules bracket the user's: local/loopback
  access and the replication rule come first (HA and the agent can never be broken),
  the scram catch-all always comes last. Because pg_hba is first-match-wins, users
  tighten access with explicit `reject` rules that match before the catch-all —
  nothing is lost, and forgetting a rule can never lock every client out.
- **Auth method allowlist: `scram-sha-256`, `reject`, `trust`.** cert needs TLS (not
  built), ldap/radius need auth options the schema does not carry, md5 is deprecated.
  The allowlist can grow later without a schema change.
- **The agent owns the entire file.** `pg_hba.conf` on disk is a pure function of the
  spec — byte-for-byte testable, independent of the PostgreSQL version, no
  parse-and-splice of initdb output.

## Spec and validation (`pglord-core`)

New module `crates/pglord-core/src/hba.rs`:

```rust
pub struct HbaRule {
    pub conn: HbaConn,        // host | hostssl | hostnossl
    pub database: String,     // "all", "replication", or a safe identifier
    pub user: String,         // "all" or a safe identifier
    pub address: String,      // CIDR ("10.0.0.0/8", "::1/128") or all | samehost | samenet
    pub method: HbaMethod,    // scram-sha-256 | reject | trust
}
```

`ClusterSpec` gains `#[serde(default)] pub pg_hba: Vec<HbaRule>`; `SpecChange` gains
`pub pg_hba: Option<Vec<HbaRule>>` with the same replace-wholesale semantics as
`parameters` (absent = untouched, present = the new complete list).

Validation (in `ClusterSpec::validate`, via `hba::validate_rules`):

- `local` connection type is not expressible — the managed prefix owns socket access,
  and a user `local` rule after `local all all trust` could never match anyway.
- `database` / `user`: `all` (and `replication` for database) or a token matching
  `[A-Za-z_][A-Za-z0-9_$]*` of at most 63 bytes. No commas, quotes, or whitespace —
  one rule, one database, one user.
- `address`: parses as an IPv4/IPv6 CIDR (`addr/prefix`) or is one of the keywords
  `all`, `samehost`, `samenet`. Plain hostnames are rejected (DNS-dependent auth is a
  reliability trap).
- At most 100 rules.

Rendering: `hba::render(rules, repl_user, scram) -> String` produces the whole file —

```
# Managed by pglord — do not edit; changes come from the cluster spec.
local   all  all                trust
host    all  all  127.0.0.1/32  trust
host    all  all  ::1/128       trust
host    replication  <repl_user>  0.0.0.0/0  scram-sha-256

# User rules (cluster spec pg_hba)
<one line per rule>

# Managed catch-all
host    all  all  0.0.0.0/0  scram-sha-256
```

The prefix preserves today's effective behavior (initdb wrote local + loopback trust).
In credential-less dev mode (`scram = false`) the replication rule and catch-all render
as `trust`, exactly like `pg_hba_extra` today. Managed rules stay IPv4-only, matching
today's `pg_hba_extra`; users who need IPv6 client access can add rules themselves.

## Agent

- **Bootstrap** (primary): after initdb, write the fully rendered file over
  `pg_hba.conf` instead of appending `pg_hba_extra`. Rules present in the spec at
  creation time are included from the first boot. `pg_hba_extra` is deleted.
- **Standby**: `pg_basebackup` copies the primary's file; the watch loop converges it,
  so no bootstrap-time write is needed beyond what already happens.
- **Watch loop**: `apply_parameters` in `record.rs` additionally renders pg_hba from
  the spec and writes it via a `write_hba_if_changed` helper (same
  compare-then-write shape as `params::write_if_changed`, but targeting
  `pg_hba.conf`). A change sets the existing `changed` flag, so the one reload at the
  end of the pass covers parameters and pg_hba together.
- **Upgrade path**: existing clusters have the initdb-plus-append file; its content
  differs from the canonical render, so the first watch-loop pass rewrites it and
  reloads once. Effective rules are equivalent.

The agent needs the spec's rules and the replication user in its watch loop; it already
reads the full spec from the ConfigMap (`reader.spec()`) and has `repl_user` in its
`Config`, so no new plumbing.

## Server and contract

No new endpoint. `pg_hba` rides the existing spec-update path; invalid rules are
rejected by core validation as the usual RFC 9457 400 with a distinct `SpecError`
variant per failure (unknown method is unrepresentable — serde rejects it at
deserialization). After the DTO change: `make openapi ui-client`, commit the
regenerated spec and client with the code.

## UI

Cluster detail, Configuration area: a "Client access rules" editor —

- Table of rules: selects for connection type and method, text inputs for database,
  user, address; add-row and remove-row; save issues the same spec-update call as the
  parameters editor and surfaces field errors from the 400 problem detail.
- A short explainer: rules are evaluated first-match-wins; pglord always allows
  replication and local access before your rules and applies a scram catch-all after
  them; use `reject` rules to tighten. Changes apply with a reload — no restart.
- All strings through Transloco (`ui/public/i18n/`).

## Testing

- **Core unit**: validation accept/reject table (CIDRs v4/v6, keywords, bad tokens,
  hostnames, >100 rules, `local` unrepresentable); `render` output asserted
  byte-for-byte for scram and trust modes, with and without user rules.
- **Agent unit**: bootstrap writes the full rendered file (no initdb remnants in the
  managed sections); `write_hba_if_changed` is idempotent — second pass with the same
  spec writes nothing and does not reload.
- **Server**: spec update with an invalid rule → 400 problem+json; OpenAPI doc
  contains the new schema.
- **e2e** (`config-e2e.sh`): on a running cluster add a `reject` rule for a test role
  → psql as that role from a client pod is refused (`pg_hba.conf rejects connection`)
  while replication keeps streaming and other clients still connect; remove the rule
  → access restored. No pod restarts observed during the change.

## Docs

User guide: new "Client access rules (pg_hba)" section next to "Tuning PostgreSQL
settings" — rule fields, first-match-wins with the fixed prefix/suffix, the
reject-to-tighten pattern, reload semantics; drop the "`pg_hba.conf` rules are not
editable yet" limitation line.
