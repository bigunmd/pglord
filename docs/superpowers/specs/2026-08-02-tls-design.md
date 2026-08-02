# TLS for Postgres connections — design

Date: 2026-08-02
Status: approved

## Goal

Encrypt client and replication traffic to pglord-managed Postgres clusters and support
client-certificate authentication, using certificates maintained outside pglord
(typically cert-manager). pglord never generates or stores key material.

## Decisions (settled with user)

| Question | Decision |
|---|---|
| Cert source | Referenced `kubernetes.io/tls` Secret only; no self-signed generation by pglord |
| mTLS | Yes — client-cert auth via pg_hba `cert` method |
| Client CA | The optional `ca.crt` key of the same referenced Secret; no separate CA field |
| Replication | Opportunistic TLS (libpq default `sslmode=prefer`); managed hba rules unchanged |
| Enforcement | No `tls.required` flag; docs show the `hostnossl all all 0.0.0.0/0 reject` recipe |

## Spec surface

New optional block on `ClusterSpec`, `#[serde(default)]` like every other feature block:

```jsonc
"tls": { "serverSecret": "mydb-tls" }
```

- TLS is enabled iff `serverSecret` is set and non-blank after trimming (accessor
  follows `InitSpec::config_map()` — a form may send `""`).
- The Secret is a standard `kubernetes.io/tls` Secret: `tls.crt`, `tls.key`, and an
  optional `ca.crt` (cert-manager writes all three).
- Mutable via `SpecChange` with replace-wholesale semantics: absent = untouched,
  empty block = TLS disabled. Enabling, disabling, and repointing on a running
  cluster are all supported.
- `hba.rs` gains `HbaMethod::Cert` (renders `cert`). Validation:
  - a `cert` rule requires conn type `hostssl`;
  - any `cert` rule requires `tls` to be enabled on the spec.
  Both are pure checks in `ClusterSpec::validate` and reject with 422 at the API.

## Dependencies (deps.rs)

New `ResourcePurpose::Tls`. When TLS is enabled the spec requires the Secret with keys
`tls.crt`, `tls.key`; `ca.crt` is added to the required keys iff any `cert` hba rule
exists. The existing `required-secrets` endpoint and UI then surface a missing or
incomplete cert Secret with no new server code.

## Rendering (render.rs)

- Volume `tls-certs` from the referenced Secret, mounted read-only at
  `/etc/pglord/tls`, `defaultMode: 0o640`.
- **Non-optional** volume (init-scripts reasoning, not backup-env reasoning): a
  missing Secret must block the pod with a clear event rather than let a cluster come
  up "healthy" without TLS.
- No init-container copy. With the existing pod `fsGroup: 999`, kubelet projects the
  files as `root:999` mode `0640`; Postgres accepts a root-owned group-readable key.
- Adding/removing the volume changes the pod template, so the existing
  `pod_template_changed` → rolling-restart machinery covers enable/disable with no
  new orchestration.

## Agent

Fourth managed conf include, `postgresql.pglord-tls.conf`, listed in
`postgresql_conf_extra` alongside the sync/archive/standby includes and rendered by
the 5s converge pass (`write_file_if_changed` pattern):

```
ssl = on
ssl_cert_file = '/etc/pglord/tls/tls.crt'
ssl_key_file  = '/etc/pglord/tls/tls.key'
ssl_ca_file   = '/etc/pglord/tls/ca.crt'   # line present only when the file exists
```

When TLS is disabled the include renders empty. All `ssl*` GUCs are reload-context
since PG 10, and the converge pass already ends in a single `pg_ctl reload` when any
managed file changed.

**Rotation (the one new mechanism):** each converge pass hashes the mounted
`tls.crt`/`tls.key`/`ca.crt` contents; a differing hash sets the shared `changed`
flag. cert-manager renews the Secret → kubelet swaps the projected files → agent
reloads within ~5s; `restartCount` unchanged. A `ca.crt` that appears later is picked
up the same way (conf gains the `ssl_ca_file` line → reload).

`RESERVED` gains `ssl`, `ssl_cert_file`, `ssl_key_file`, `ssl_ca_file`,
`ssl_crl_file` (archive_* precedent: the managed feature owns them).
`ssl_min_protocol_version` / cipher settings stay out for now and can join the user
catalog later.

## Replication

No changes. libpq defaults to `sslmode=prefer`, so standby→primary streaming,
`pg_basebackup`, and `pg_rewind` encrypt automatically once the primary offers TLS
and keep working against a cert-less peer while a cluster rolls into (or out of) TLS.
Managed hba rules stay `host`; auth stays scram.

## API / UI

- `SpecChange` + cluster-detail DTO gain the `tls` block; agent `/status` gains
  `ssl: bool` (from `SHOW ssl`) so the UI can badge instances.
- Contract pipeline as usual: `make openapi ui-client`, commit regenerated artifacts.
- UI: TLS dialog on cluster detail (set/clear secret, fed by required-secrets),
  `cert` in the hba dialog method dropdown, TLS field on the create form.
- No chart/RBAC changes: the server already reads Secrets namespace-wide; agents
  never read Secrets via the API (the cert reaches them as a volume).

## Error handling

| Failure | Behavior |
|---|---|
| Secret missing / wrong keys | required-secrets flags it; pod blocks pending (non-optional volume) with a clear event |
| Bad cert content on rotation | Postgres logs the reload failure and keeps serving the old cert (native behavior) |
| Bad cert on first enable | Pod start fails visibly |
| `cert` rule without CA | 422 if TLS disabled; otherwise `ca.crt` marked required in deps and surfaced |

## Testing

- **Core:** volume/mount render on/off, conf-include render with/without `ca.crt`,
  validation matrix for `cert` rules (conn type, TLS off), deps key sets.
- **Agent:** include rendering, hash-change → reload flag.
- **E2E** (`scripts/tls-e2e.sh`, `make tls-e2e`, config-e2e shape): openssl
  self-signed Secret → create cluster → `psql "sslmode=require"` succeeds and
  `pg_stat_ssl` shows TLS → rotate the Secret, assert the new cert is served with
  `restartCount` unchanged → enable TLS on a running non-TLS cluster, assert the
  roll → `hostnossl … reject` rule blocks plaintext.
- **Docs:** user-guide TLS section with a cert-manager `Certificate` example and the
  hostnossl-reject recipe.

## Out of scope (deliberate)

Self-signed cert generation by pglord · mTLS for replication · TLS protocol/cipher
tuning knobs · CRLs · separate client-CA reference.
