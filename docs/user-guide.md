# pglord user guide

pglord runs highly-available PostgreSQL clusters in your Kubernetes namespace.
You create a cluster from a web UI; pglord provisions the instances, keeps one
primary and one or more streaming standbys, and promotes a standby automatically
if the primary fails.

Everything lives in a single namespace. You do not need cluster-admin.

---

## 1. Install

You need: access to a Kubernetes namespace, and the two pglord images
(`pglord-server`, `pglord-agent`) available to your cluster — from your registry,
or built locally with `make image agent-image` for local clusters like minikube.

### With Helm (recommended)

```bash
helm install pglord ./charts/pglord \
  --namespace pglord --create-namespace \
  --set image.repository=<registry>/pglord-server \
  --set agentImage=<registry>/pglord-agent:0.1.0
```

**Set `agentImage`.** It is the image your Postgres instances run, used whenever
a cluster is created without naming one. The built-in default,
`pglord-agent:dev`, exists only on a development machine — leave it in place on
a real cluster and every cluster you create sits in `ImagePullBackOff`.

Installing into a different namespace needs nothing more than `--namespace`:
pglord is namespaced throughout, so two installations in two namespaces are
independent. See `charts/pglord/README.md` for every value.

### With plain manifests

```bash
kubectl apply -f deploy/k8s/
kubectl -n pglord rollout status deploy/pglord-server
```

This creates the `pglord` namespace, a ServiceAccount with a namespace-scoped
Role, the pglord Deployment, and its Service, all fixed to the `pglord`
namespace. Change `namespace:` in the manifests to install elsewhere.

Open the UI:

```bash
kubectl -n pglord port-forward svc/pglord 8080:80   # helm install
kubectl -n pglord port-forward svc/pglord-server 8080:80   # plain manifests
# http://localhost:8080
```

For shared access, expose the Service through your Ingress controller instead of
port-forwarding (`--set ingress.enabled=true` with the chart). pglord has no
built-in authentication yet: anyone who can reach it can create and delete
clusters, and deleting a cluster removes its data volumes. Put it behind your
ingress auth, and do not expose it publicly.

### Uninstalling

`helm uninstall` removes the control plane but **not** your Postgres clusters:
their StatefulSets, Services, Secrets and volumes are owned by the spec
ConfigMaps, not by the Helm release, so they keep running without a UI. Delete
clusters through the UI first if that is what you meant.

## 2. Check permissions

Open **Permissions** in the UI. It asks the Kubernetes API server what pglord is
actually allowed to do and lists every required permission as granted, denied, or
unknown.

If anything is denied, cluster management will fail. Ask whoever administers the
namespace to apply `deploy/k8s/rbac.yaml` (or install the chart with
`rbac.create=true`, the default), then reload the page. The same report is
available at `GET /api/v1/preflight`, and the server logs a summary at startup.

Worth checking after an upgrade too: a denied row here is the difference between
an operation failing with a clear reason and one failing at the moment you need
it. Growing storage, for instance, patches the volume claims and needs `patch`
on `persistentvolumeclaims`.

## 3. Create a cluster

**Clusters → Create cluster.**

| Field | Meaning |
|---|---|
| Name | Becomes the prefix of every object: pods `<name>-0`, `<name>-1`, services `<name>-rw`, `<name>-pods`. Lowercase letters, digits, hyphens; max 40 characters. |
| Instances | Total instances: 1 primary plus streaming standbys. Two or more gives you automatic failover; one is a single instance with no redundancy. |
| Storage per instance | Size of the persistent volume for each instance, e.g. `1Gi`. |
| Image | The pglord agent image. Leave the default unless you build your own. |

The cluster appears immediately; the instance dots turn green as each instance
finishes bootstrapping (typically 30–60 seconds — the first instance initializes
a database, the others clone it).

Click the row to open the cluster: which instance is primary, per-instance
health, and each instance's WAL position.

## 4. Connect an application

Every cluster publishes two in-cluster addresses:

| Address | Use |
|---|---|
| `<name>-rw.<namespace>.svc.cluster.local:5432` | Reads and writes. Always points at the current primary, including after a failover. |
| `<name>-pods.<namespace>.svc.cluster.local:5432` | All instances (headless). Individual instances are `<name>-0.<name>-pods…`, useful for read-only queries against a specific standby. |
| `<name>-pool.<namespace>.svc.cluster.local:5432` | Pooled reads and writes through PgBouncer, when the pooler is enabled (see [Connection pooling](#connection-pooling-pgbouncer)). |

Credentials are generated per cluster and stored in a Secret named
`<name>-credentials`, with keys `superuser-password` and `replication-password`.
The superuser is `postgres`; the database is `postgres`.

Read the password:

```bash
kubectl -n pglord get secret <name>-credentials \
  -o jsonpath='{.data.superuser-password}' | base64 -d
```

Connect with `psql` from inside the cluster:

```bash
kubectl -n pglord run psql --rm -it --restart=Never --image=postgres:17 -- \
  psql "postgresql://postgres@<name>-rw:5432/postgres"
# paste the password when prompted
```

Point an application at it — mount the password from the Secret rather than
copying it into your manifests:

```yaml
env:
  - name: PGHOST
    value: <name>-rw.pglord.svc.cluster.local
  - name: PGPORT
    value: "5432"
  - name: PGUSER
    value: postgres
  - name: PGDATABASE
    value: postgres
  - name: PGPASSWORD
    valueFrom:
      secretKeyRef:
        name: <name>-credentials
        key: superuser-password
```

If your application runs in a different namespace, use the fully-qualified host
above and copy the Secret into that namespace.

**Create your own user and database** rather than shipping with the superuser:

```sql
CREATE ROLE app LOGIN PASSWORD 'choose-a-password';
CREATE DATABASE orders OWNER app;
```

## 5. What failover looks like

When the primary becomes unreachable, pglord promotes the most up-to-date
standby (typically within a minute) and repoints `<name>-rw` at it. The cluster
detail page shows the primary marker move to the new instance. When the old
primary comes back, it rejoins automatically as a standby.

From your application's side, a failover looks like a dropped connection.
Applications should retry on connection errors and use a connection pool that
reconnects — that is all that is needed to ride through a failover.

Uncommitted transactions on the old primary are lost. On an asynchronous
cluster (the default), a small window of recently-committed writes can also be
lost if the primary fails before a standby receives them — synchronous
replication (below) closes that window.

## 6. Synchronous replication

Turn on **Synchronous replication** on the cluster page (or set
`synchronous: true` on the spec) and every commit waits until at least one
standby confirms it has the WAL on disk. pglord manages
`synchronous_standby_names` as a quorum — `ANY 1` over all of the cluster's
standbys — so any one of them can acknowledge. The change applies with a
reload: no restart, no rolling of instances, and it can be switched off the
same way.

What you gain: a primary failure no longer loses committed transactions. The
promoted standby is the most advanced one, and with quorum sync every commit
was confirmed by at least one standby — only the primary and that confirming
standby failing together can lose the tail.

What you trade: **if no standby is connected, writes block** until one
returns. pglord is strict about this on purpose — it never silently falls
back to asynchronous, because that would void the guarantee exactly when it
matters. When commits are blocking, the cluster page shows a warning banner,
the cluster detail reports `sync_degraded: true`, and the server exports
`pglord_sync_degraded 1` for alerting.

Practical notes:

- Requires at least 2 instances; scaling a synchronous cluster down to 1 is
  rejected.
- With exactly 2 instances, a rolling restart briefly blocks writes while the
  only standby restarts. Three or more instances keep a quorum standby
  available through a roll.
- `synchronous_commit` stays tunable, but `off` and `local` are rejected while
  the flag is on (either would defeat it). Set `remote_apply` if you want
  standbys to also have *applied* each commit before it returns —
  read-your-writes on the `-ro` Service.

## 7. Day-2 operations

Open a cluster from the Clusters list to reach these.

### Switch the primary on purpose

Before draining a node, upgrading it, or moving load off an instance, hand the
primary role to a specific standby: **Make primary** on that instance's row,
then confirm. The old primary steps down, the chosen instance takes over, and
`<name>-rw` follows within seconds.

Only ready standbys offer the button. Promoting an instance that is not
streaming would lose whatever WAL it never received, so pglord refuses.

As with a failover, applications see a brief dropped connection and reconnect.

### Add or remove instances

**Edit → Instances.** Adding an instance provisions it and clones the current
primary automatically; it starts streaming when the clone finishes.

Removing instances **deletes their data volumes**. Only the highest-numbered
instances are removed, and the primary is never one of them.

### Minor version upgrades

Click the pencil next to the image name in the cluster header, enter the new
image, and confirm. pglord rolls every instance onto it: standbys first, the
primary last, one at a time, each waited on until it is ready again — the same
order as a rolling restart, so writes stay available for as long as possible.

Rules and recovery:

- **Same PostgreSQL major only.** The image tag itself is not inspected (tags
  are arbitrary), but an instance refuses to start when its data directory was
  created by a different major version, with a log line naming both versions.
  Revert the image in the spec and the cluster rolls back.
- **A bad image** (unpullable, broken) stops the roll at the first standby;
  the primary keeps serving on the old image. Revert the image to roll back.
- **If the pglord server restarts mid-roll**, the remaining instances stay on
  the old image (the StatefulSet only replaces pods pglord deletes). Use
  **Restart cluster** to finish the roll.

Changing custom environment variables or workload settings rolls instances
the same way.

### Back up to object storage

pglord backs clusters up with pgBackRest to any S3-compatible storage.

**Before you start**, create a Secret in the cluster's namespace holding the
object-storage credentials. pglord stores only its *name*; the keys never enter
a cluster spec.

```bash
kubectl -n pglord create secret generic backup-credentials \
  --from-literal=access-key=AKIA... \
  --from-literal=secret-key=...
```

Then **Backups → Configure repository**: endpoint (a full URL), bucket, region,
the Secret name, an optional path prefix so several clusters can share a bucket,
and how many full backups to keep — pgBackRest expires older ones for you.

Enabling backups turns on WAL archiving, which PostgreSQL only picks up on
restart, so the cluster shows a pending restart afterwards. Restart it, then use
**Back up now** to take the first backup and confirm the repository works.
Archiving stays off while no repository is configured — otherwise WAL would
accumulate on disk with nowhere to ship it.

Backups always run on the primary.

**Backup types.** *Full* copies everything and is the base the others build on.
*Differential* copies what changed since the last full backup. *Incremental*
copies what changed since the last backup of any type — smallest and quickest.
Restoring always needs the full backup plus the chain that follows it, so a
schedule of incrementals still depends on taking a full one regularly.

**On a schedule.** Give the repository a cron expression (UTC) and the type to
take — for example `0 3 * * *` for 03:00 daily, or `0 3 * * 0` for Sundays.
Ordinary five-field cron works as you would write it in a crontab. The shortest
interval allowed is 15 minutes, since backups that overlap only cost storage. If
pglord is down when a schedule fires, it takes one catch-up backup when it comes
back rather than one for every window it missed.

**Two lists, deliberately.** *Recent runs* shows what happened, including
backups still running and ones that failed with the reason. *In the repository*
shows what actually exists and could be restored. A failed run leaves nothing in
the repository, which is exactly why both are shown. Only one backup of a
cluster runs at a time; asking for another while one is in flight is refused
rather than queued.

### Restore a backup

Every backup row has a **Restore** button. A restore always creates a **new
cluster** and never touches the one you restored from — usually you are trying
to rescue data, and writing an older copy over the live cluster would destroy
the only thing you still have. Restoring onto the source's own name is refused.

Two things you can restore to:

- **A backup** — the data exactly as that backup captured it, plus every change
  archived afterwards. Restoring the newest backup gives you the cluster as it
  was moments before the repository last received WAL.
- **A point in time** — the write-ahead log is replayed up to the moment you
  name and then stops. This is what recovers from a bad migration or a
  `DELETE` without a `WHERE`: pick a time just before it happened. The moment
  has to fall inside the range your backups and archived WAL cover, and times
  are interpreted in UTC.

The new cluster inherits the source's image, size, storage and settings, so what
comes up is shaped like what you lost. Two things are deliberately *not*
inherited:

- **Backups are off on the restored cluster.** It would otherwise write into the
  same repository path it was restored from, where a stanza for a different
  database already lives. Give it a repository of its own — a different path
  prefix — once you are satisfied with the data.
- **Its passwords are its own.** The restored files carry the source cluster's
  roles, so pglord resets the superuser and replication passwords to the ones in
  the new cluster's own Secret. Read them the same way as for any cluster.

The restored cluster shows where it came from at the top of its page. Restoring
takes as long as the data needs to come back from object storage and replay, so
the cluster sits not-ready until that finishes; if it never becomes ready, the
instance's logs carry pgBackRest's own error.

### Placement and resources

**Placement and resources → Edit placement.**

**Instance spreading** decides whether two instances of a cluster may share a
node. The default, *Preferred*, spreads them across nodes when nodes are
available and still schedules when they are not — which is what you want on a
small or single-node cluster. Switch to *Required* on a multi-node cluster if
you want a hard guarantee: instances will never share a node, and any instance
that cannot find a free node stays pending rather than co-locating. *Off*
removes the rule entirely; a single node failure can then take the whole
cluster down.

**Resources** set CPU and memory requests and limits per instance. Requests
matter most: without them the instances are best-effort and are the first thing
Kubernetes evicts when a node runs short of memory. A request also reserves
capacity so the scheduler will not overcommit the node your database is on. Use
quantities like `100m`, `1`, `256Mi`, `2Gi`; leave a field blank to set none.

**Labels and annotations** are added to instance pods — useful for cost
allocation, Prometheus scrape selection, service-mesh injection and backup
tooling. Keys under `app.kubernetes.io/`, `pglord.dev/` and
`statefulset.kubernetes.io/` are refused, because pglord uses them to find its
own objects.

Changing placement or resources rolls the instances one at a time, the same way
a restart does.

Also available through the API (`workload` on create or `PATCH`): `node_selector`
and `tolerations` for dedicated database node pools, `image_pull_secrets` for
private registries, `priority_class_name` for eviction protection, and
`storage_class` to pick a StorageClass. The StorageClass can only be set when
the cluster is created — Kubernetes does not allow changing it afterwards.

### Tune PostgreSQL settings

**Configuration → Edit configuration.** Pick a setting, give it a value, and
apply. pglord tells you before you save whether the setting takes effect
immediately or needs a restart:

- **applies live** — written to every instance and reloaded within seconds. No
  disruption. Most settings are in this group: `work_mem`,
  `log_min_duration_statement`, autovacuum tuning, planner costs.
- **needs restart** — staged, but PostgreSQL keeps running with the old value.
  Examples: `shared_buffers`, `max_connections`, `max_worker_processes`.

When something is staged, the cluster shows a banner with a **Restart cluster**
button. The restart is rolling: standbys first, the primary last, one instance
at a time, so the cluster stays available apart from brief reconnects.

Removing a setting from the list returns it to the PostgreSQL default.

Two guardrails: only settings pglord knows are accepted (a typo is rejected
rather than left to break startup), and settings pglord manages itself —
`wal_level`, `max_wal_senders`, `hot_standby`, `wal_log_hints`,
`listen_addresses`, `port` — cannot be overridden, because replication and
failover depend on them.

Lowering `max_connections`, `max_worker_processes`,
`max_prepared_transactions` or `max_locks_per_transaction` is safe at any
time: each instance picks the lower value up at its next restart, and until
the primary has restarted, standbys keep running with the primary's higher
live value (pglord raises it for them automatically — PostgreSQL refuses
standby recovery below the primary's live setting, so this is what keeps
replicas rebuildable in the meantime).

### Client access rules (pg_hba)

Each cluster carries a list of `pg_hba.conf` rules, edited on the cluster page
(**Client access rules → Edit access rules**) or via
`PATCH /api/v1/clusters/{name}` with a `pg_hba` array. A rule has a connection
type (`host`, `hostssl`, `hostnossl`), a database (`all`, `replication`, or
one database name), a user (`all` or one role name), an address (a CIDR like
`10.0.0.0/8` or `::1/128`, or `all`, `samehost`, `samenet`), and an auth
method (`scram-sha-256`, `reject`, `trust`).

PostgreSQL evaluates pg_hba rules first match wins, and pglord fixes the ends
of the file: replication and local socket access always come first (so a rule
cannot break HA or the agent), and a catch-all accepting any
password-authenticated client always comes last. Your rules sit in between —
which means they mostly matter for *tightening*: a `reject` rule matches
before the catch-all and turns clients away. Example: reject one role from
everywhere, and everyone else stays implicitly allowed:

    host  all  contractor  0.0.0.0/0  reject

Changes apply with a configuration reload — connections already established
are not re-checked, and no instance restarts. Rules apply identically on every
instance, primary and standbys, and a restored cluster inherits its source's
rules. Rule changes are validated up front; a rule that does not pass (bad
CIDR, unknown method, hostname addresses) is rejected with HTTP 422 and
applies nothing.

With [TLS enabled](#tls-for-client-connections) two more things become useful:
`hostssl`/`hostnossl` rules actually distinguish encrypted from plaintext
connections, and the `cert` method authenticates clients by certificate
instead of password (see below).

### TLS for client connections

pglord never generates certificates. You point a cluster at a standard
`kubernetes.io/tls` Secret — keys `tls.crt` and `tls.key`, optionally
`ca.crt` — and pglord mounts it into every instance and configures
PostgreSQL to serve it. cert-manager produces exactly this kind of Secret:

```yaml
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: orders-db-tls
spec:
  secretName: orders-db-tls
  issuerRef:
    name: my-issuer
    kind: ClusterIssuer
  dnsNames:
    - orders-db-rw
    - orders-db-rw.<namespace>.svc
    - orders-db-rw.<namespace>.svc.cluster.local
```

Enable it on the cluster page (**TLS → Configure TLS**), in the create
dialog's advanced section, or via `PATCH /api/v1/clusters/{name}` with
`{"tls": {"server_secret": "orders-db-tls"}}`. An empty block
(`{"tls": {}}`) disables TLS again. Enabling, disabling or repointing changes
the pod template, so the instances roll one at a time — standbys first, the
primary last. The Secret must exist before the roll: pods deliberately wait
on a missing certificate rather than starting without the TLS they were told
to serve (the Secrets table on the cluster page shows what is missing).

Clients connect with their usual `sslmode` settings — `require` for
encryption, `verify-full` with the CA to also authenticate the server.
Replication between instances encrypts automatically once the primary serves
a certificate (libpq's default `sslmode=prefer`), and keeps working while a
cluster rolls into or out of TLS.

**Rotation is automatic.** When cert-manager renews the Secret, every
instance picks the new certificate up within a few seconds with a
configuration reload — no restart, nothing to do. The lock icon in the
instances table shows which instances are serving TLS.

**Requiring TLS** is a pg_hba decision, not a switch: user rules run before
the managed password catch-all, so one rule turns every plaintext client
away while encrypted ones proceed:

    hostnossl  all  all  0.0.0.0/0  reject

**Client certificates (mTLS).** If the Secret carries a `ca.crt` key,
PostgreSQL verifies client certificates against that CA, and `cert` becomes
available as a pg_hba method: the connection must present a valid client
certificate whose CN equals the database role, and no password is asked.
`cert` rules are only accepted on `hostssl` lines and only while TLS is
enabled; with such rules present, `ca.crt` shows up as a required key in the
Secrets table.

    hostssl  all  app_rw  10.0.0.0/8  cert

A restored cluster inherits its source's TLS block along with its pg_hba
rules. The certificate's DNS names still name the source's services, so
`verify-full` clients need a certificate issued for the new name — the wire
stays encrypted in the meantime.

### Connection pooling (PgBouncer)

**Configure pooling** on the cluster page (or the pooler block of the create
form) runs a managed PgBouncer deployment in front of the primary.
Applications switch their host from `<name>-rw` to `<name>-pool` — same port
5432, same credentials — and connection-hungry apps stop exhausting
`max_connections`.

```
<name>-pool.<namespace>.svc.cluster.local:5432
```

Any database role connects through the pool with its own password — including
roles created by your init scripts. PgBouncer looks passwords up live in
PostgreSQL (`auth_query` through a dedicated `pglord_pooler` role that pglord
provisions and rotates); no role list to maintain, and no password ever lands
in the pooler's configuration except its own.

The knobs, all pglord-curated (everything else in pgbouncer.ini is managed):

| Knob | Default | Meaning |
|---|---|---|
| Pooler pods | 1 | PgBouncer replicas, independent of postgres instances |
| Pool mode | transaction | `transaction` returns a server connection at each transaction end; `session` keeps one per client |
| Max client connections | 100 | Client connections PgBouncer accepts |
| Pool size per database/user | 20 | Server connections per database/user pair |

**Transaction mode caveat:** session state — prepared statements, advisory
locks, `SET` without `LOCAL` — does not survive across transactions. Apps
that rely on it should use session mode.

Changing pooler knobs rolls only the pooler pods; the postgres instances are
never touched. Disabling the pooler deletes its Deployment, Service and
config Secret (the `pglord_pooler` role stays in PostgreSQL — harmless, and
dropping it would break in-flight connections). On failover the pool follows
`<name>-rw` automatically; in-flight connections break once, exactly as they
do for direct clients.

**With TLS enabled** the pooler serves the same certificate as the instances
and connects upstream with TLS required, so `hostnossl … reject` setups keep
working through the pool. Two notes: add `<name>-pool` to the certificate's
DNS names (`verify-full` clients check it), and PgBouncer does not hot-reload
certificates — after a cert renewal, restart the pooler pods:

```bash
kubectl -n pglord rollout restart deployment/<name>-pooler
```

If you supply your own credentials Secret, add a `pooler-password` key before
enabling the pooler — the Secrets table on the cluster page lists it as
required.

### Roles and databases

**Edit roles & databases** on the cluster page declares application roles and
databases that pglord creates and keeps in line on the primary — after
failovers and restores too. Unlike init scripts, declarations converge:
edit them any time and the change lands within a reconcile pass, with no
restarts.

```jsonc
"roles": [
  { "name": "reporting", "login": false },
  { "name": "app_rw",
    "password_secret": { "name": "app-creds", "key": "password" },
    "in_roles": ["reporting"] }
],
"databases": [
  { "name": "appdb", "owner": "app_rw" }
]
```

Passwords come from Secrets you own — the spec never carries one. Create the
Secret first (it appears in the cluster's Secrets table as required):

```bash
kubectl -n pglord create secret generic app-creds --from-literal=password='s3cret'
```

To rotate a role's password, edit the Secret; the next reconcile pass applies
it. Roles may set `login`, `createdb`, `createrole`, `connection_limit` and
`in_roles` (memberships to other declared roles). Databases name a declared
role as owner. Names must be lowercase identifiers; `postgres`, `replicator`,
`pglord_pooler`, `pg_*` and the template databases are reserved.

**The spec is additive.** Removing an entry stops managing it — the role or
database stays, with its data. pglord never drops anything; deletions remain a
deliberate manual operation. Declared roles authenticate through the
[connection pooler](#connection-pooling-pgbouncer) with no extra
configuration.

For anything beyond roles and databases — schemas, grants, extensions, seed
data — use init scripts or run SQL directly.

### Grow storage

**Edit → Storage per instance.** Storage can only grow — Kubernetes cannot
shrink a volume — and your StorageClass must have `allowVolumeExpansion: true`.
If it does not, the change is rejected and nothing is modified. Check with:

```bash
kubectl get storageclass -o custom-columns=NAME:.metadata.name,EXPANSION:.allowVolumeExpansion
```

Depending on the storage driver, a filesystem resize may only complete when the
instance restarts.

Changing the cluster name or image after creation is not supported.

### Secrets, environment and initialisation

**Secrets and initialisation** on the cluster page lists every Secret and
ConfigMap the cluster depends on, the keys each must contain, and whether it is
actually there. Anything marked *Created by pglord* you can ignore; anything
marked *Missing* you have to create, and instances will not start until you do.
This is the fastest way to find out why a pod is stuck in
`CreateContainerConfigError` — the reason is otherwise buried in pod events.

**Use your own credentials Secret.** By default pglord generates
`<cluster>-credentials`. To supply your own instead, create a Secret with keys
`superuser-password` and `replication-password` and name it when you create the
cluster:

```bash
kubectl -n pglord create secret generic orders-creds \
  --from-literal=superuser-password='...' \
  --from-literal=replication-password='...'
```

Passwords may contain letters, digits, `-` and `_`. This is **fixed once the
cluster exists**: repointing a running cluster at a different Secret would
change the password it presents without changing the one stored in the
database, locking out every client while the cluster still looked healthy.
Rotating the managed passwords is supported — see below.

### Rotating credentials

For clusters whose credentials pglord generated (no user-supplied Secret),
**Rotate passwords** on the cluster page — or
`POST /api/v1/clusters/<name>/rotate-credentials` — replaces both the
superuser and replication passwords in one operation: new values are written
to the `<name>-credentials` Secret, applied on the primary, and every
instance is restarted one at a time, standbys first. The Secret's
`pglord.dev/rotated-at` annotation records when this last happened.

Connected clients keep working until they reconnect; after that they must
present the new password from the Secret, so update anything that caches it.
Clusters that reference your own Secret are refused (`409
custom_credentials_secret`): pglord does not write to Secrets it does not
own.

The new passwords travel from the server to the primary's agent over
unauthenticated in-cluster HTTP — the same channel the failover promote call
uses. If that is not acceptable in your environment, treat rotation (and
pglord's control plane generally) as trusted-namespace-only.

If a rotation fails partway (the run counter
`pglord_credential_rotations_total{outcome="failed"}` ticks and the log says
so), simply POST it again: every step is idempotent and a re-run converges
the cluster.

**Custom environment variables** are set on every instance, either as literal
values or read from a Secret. Names pglord uses itself are refused —
`PGLORD_AGENT_*`, `PGBACKREST_*`, and libpq's `PGDATA`, `PGHOST`, `PGUSER` and
friends. That is not tidiness: `PGLORD_AGENT_ROLE` on every instance would
declare them all primaries, and `PGDATA` would point postgres at a different
directory.

**Initialisation scripts** run once, on the primary, when the data directory is
first created. Put `.sql` files in a ConfigMap and name it on the cluster; they
run in filename order, so name them `01-…`, `02-…`. Use a Secret instead (or as
well) for SQL containing passwords — a ConfigMap is readable by anyone with
namespace read access. Secret scripts run after ConfigMap ones. For roles and
databases specifically, prefer [declared roles and
databases](#roles-and-databases) — unlike init scripts, they keep converging
after creation.

Custom environment variables are available to scripts as psql variables, which
is how a script uses a password without it ever being written down:

```sql
-- 01-app.sql, with APP_PASSWORD set from a Secret
CREATE ROLE app LOGIN PASSWORD :'APP_PASSWORD';
CREATE DATABASE orders OWNER app;
```

```bash
kubectl -n pglord create configmap orders-init --from-file=01-app.sql
```

Three things worth knowing:

- Scripts run **only at first creation**. Adding them to an existing cluster
  does nothing — they describe how a database is created, not how it is
  migrated. Use a migration tool for changes to a live database.
- A **restored cluster never runs them**: its data already exists.
- A failing script stops the run and is logged; it is retried when the instance
  restarts, so nothing is silently half-applied. Write them to be repeatable
  (`CREATE ... IF NOT EXISTS`) if you want a retry to succeed.

### Hibernate a cluster

**Hibernate** in the cluster header stops every instance and scales the cluster
to zero pods. Data volumes, credentials and configuration are all kept — only
the compute goes away. A hibernated cluster shows a distinct *Hibernated* state
(not a red outage strip) and a **Wake** button that brings back exactly the
instances it had, with the data intact.

While hibernated, pglord refuses everything that assumes running instances
with `409 hibernated`: editing the spec, switchover, rolling restart,
credential rotation, taking a backup, fencing. Wake the cluster first. Deleting
a hibernated cluster works and still removes its volumes.

Two interactions worth knowing:

- **Backups.** Scheduled backups pause while asleep. After wake, the scheduler
  runs at most one catch-up backup rather than one per missed window.
- **Failover.** The failover orchestrator ignores hibernated clusters, so the
  intentional zero-instances state is never "repaired".

### Fence an instance

**Fence** on a standby's row stops PostgreSQL on that instance and keeps it
stopped — the pod stays up, so the data directory can be inspected
(`kubectl exec`) while the rest of the cluster keeps serving. The instance
shows a *Fenced* badge, drops out of the read Service, and is never considered
for promotion. **Unfence** restarts it; it rewinds if needed and rejoins as a
streaming standby.

The recorded primary cannot be fenced (`409 cannot_fence_primary`): fencing it
would stop writes for the whole cluster behind an innocuous-looking button.
To take the primary down, **Make primary** on another instance first, then
fence the ex-primary once it has stepped down.

Fenced state survives pod restarts and even hibernation: a fenced instance
wakes up still fenced, postgres held down, until it is unfenced.

## 8. Delete a cluster

**Clusters → trash icon → Delete.** This removes the instances, their persistent
volumes, and the generated credentials. It cannot be undone; take a dump first if
you need the data:

```bash
kubectl -n pglord exec <name>-0 -- \
  env PGPASSWORD=<password> pg_dump -U postgres postgres > backup.sql
```

If the cluster has a backup repository, restoring from it into a new cluster
(see *Restore a backup*) recovers this data too — but only up to the last
archived WAL, and only while the repository itself survives. Deleting the
cluster does not delete its repository, so a restore stays possible afterwards;
a dump is still the surer option when you want a single file in your own hands.

## 9. Troubleshooting

| Symptom | Likely cause and fix |
|---|---|
| Permissions page shows denied entries | The namespace Role is missing rules. Apply `deploy/k8s/rbac.yaml`. |
| Instances stay at 0 ready, pods `Pending` | No storage available. Check `kubectl -n pglord get pvc` and whether your cluster has a default StorageClass. |
| Instances stay at 0 ready, pods `ImagePullBackOff` | The agent image is not reachable from the cluster. Push it to your registry and set that image when creating the cluster. |
| An instance shows role "Unknown" | Its agent is unreachable. Check `kubectl -n pglord logs <name>-<n>`. |
| Cluster list shows "No Kubernetes environment" | The server has no cluster access — usually a missing ServiceAccount on the Deployment. |
| A standby never becomes ready | It is still cloning, or cannot reach the primary. Check its logs for `pg_basebackup`. |
| Pods stuck in `CreateContainerConfigError` or `ContainerCreating` | A Secret or ConfigMap the cluster references is missing, or lacks a key. Check *Secrets and initialisation* on the cluster page. |
| Init scripts did not run | They run only when a cluster is first created, never on an existing or restored one. Check `kubectl -n pglord logs <name>-0` for `init script` lines. |

## 10. Monitoring

pglord exposes Prometheus metrics in two places, and both are on by default —
there is nothing to enable.

**Every postgres instance** serves metrics on its agent port,
`:8008/metrics`. The set is deliberately small: enough to alert on the things
that page you, without the hundreds of series a full exporter ships.

| Metric | Meaning |
|---|---|
| `pglord_pg_up` | 1 if the agent could query postgres. This is the one to alert on. |
| `pglord_instance_primary` | 1 on the primary, 0 on a standby. A cluster whose sum is not exactly 1 has a problem. |
| `pglord_pg_wal_lsn_bytes` | WAL position as a number — write position on the primary, replay position on a standby. Subtract a standby's value from the primary's to graph lag in bytes. |
| `pglord_pg_replication_lag_seconds` | Standby only: seconds since the last replayed transaction. |
| `pglord_pg_standby_replay_lag_bytes{standby}` | Primary only: replay lag per connected standby. |
| `pglord_pg_connections{state}` / `pglord_pg_max_connections` | Backends by state, and the ceiling they run into. |
| `pglord_pg_database_size_bytes{database}` | Size per database. |
| `pglord_pg_pending_restart` | 1 if settings are staged that only a restart will apply. |

The endpoint always answers 200: when postgres is down the scrape still works
and reports `pglord_pg_up 0`, which is exactly when you want the data point.

**The pglord server** serves control-plane metrics on `:8080/metrics`:

| Metric | Meaning |
|---|---|
| `pglord_clusters` | Number of cluster specs. |
| `pglord_reconcile_total{cluster, outcome}` | Reconcile passes; a growing `outcome="error"` rate means a cluster cannot be applied. |
| `pglord_failover_total{cluster, trigger}` | Primary record changes — `auto` for orchestrator failovers, `manual` for switchovers. |
| `pglord_backup_runs_total{cluster, kind, outcome}` | Finished backup runs; alert on `outcome="failed"`. |
| `pglord_cluster_instances_ready{cluster}` / `pglord_cluster_instances{cluster}` | Ready instances versus how many the spec asks for. |

Instance identity is deliberately not baked into the metrics: the pod,
namespace and cluster labels come from your Prometheus discovery, so relabeling
works the way it does for every other workload.

**Discovery.** Instance pods and the server pod carry the conventional
annotations (`prometheus.io/scrape: "true"`, `prometheus.io/port`,
`prometheus.io/path`), so annotation-based scrape configs pick them up with no
extra setup. To opt a cluster out, set `prometheus.io/scrape: "false"` in its
workload annotations — a user annotation always wins over the default. If you
run prometheus-operator, the Helm chart can create the objects for you:
`metrics.podMonitor.enabled=true` scrapes every pglord-managed postgres pod and
`metrics.serviceMonitor.enabled=true` scrapes the server. Both are off by
default because they require the monitoring.coreos.com CRDs; the raw
`deploy/k8s/` manifests ship annotations only.

## 11. Authentication

Out of the box the API and UI are **open**: anyone who can reach the server
has full control, and the server says so loudly in its startup log. Two
authentication modes can be enabled, separately or together; both arrive as
`Authorization: Bearer <credential>` headers, so `curl` and CI work the same
way the UI does. `/healthz`, `/readyz` and `/metrics` always stay open.

### Static API tokens

Set `PGLORD_AUTH_TOKENS` to one or more comma-separated tokens. With the Helm
chart, point it at a Secret you manage:

```bash
kubectl -n pglord create secret generic pglord-auth \
  --from-literal=tokens="$(openssl rand -hex 32)"
helm upgrade pglord charts/pglord --reuse-values \
  --set auth.tokens.existingSecret=pglord-auth
```

The UI shows a token prompt on its sign-in screen; scripts send the header
directly:

```bash
curl -H "Authorization: Bearer <token>" http://pglord.example.com/api/v1/clusters
```

**Rotation without downtime:** add a second token to the Secret
(`tokens: "new-token,old-token"`), roll every client to the new token, then
remove the old one. Both are valid while the migration runs.

### OIDC single sign-on

pglord validates JWT access tokens issued by your identity provider
(Keycloak, Dex, Auth0, Entra ID, …); the UI runs the standard
authorization-code + PKCE flow as a public client. The server never talks to
the IdP except to fetch its published signing keys, needs no client secret,
and stays stateless.

At the IdP, register a **public** client (no secret) with redirect URI
`<ui-origin>/login/callback` — for Keycloak: client type *OpenID Connect*,
*Standard flow* on, *Client authentication* off, and the pglord origin in
*Web origins* (the UI calls the token endpoint from the browser).

Then configure the server:

```bash
helm upgrade pglord charts/pglord --reuse-values \
  --set auth.oidc.issuer=https://keycloak.example.com/realms/infra \
  --set auth.oidc.clientId=pglord
```

`auth.oidc.audience` defaults to the client ID; set it when your IdP puts a
different `aud` claim into access tokens. Tokens are accepted when their
signature matches the issuer's JWKS and `exp`, `iss` and `aud` check out.
Any valid credential has full admin access — there are no roles yet.

For non-interactive API access with OIDC, either configure a static token
alongside it, or use your IdP's client-credentials grant and send the
resulting access token as the bearer.

## Current limitations

- Authentication is all-or-nothing: any valid credential is a full admin.
  There are no viewer/operator roles yet.
- A restore always creates a new cluster; restoring in place is not supported,
  and the restored cluster starts with backups switched off.
- Custom images must be built on the pglord agent image; stock `postgres:NN`
  images are not usable yet.
- Cluster name cannot be changed after creation (instances, storage, image
  and PostgreSQL settings can — see Day-2 operations).
- Upgrades stay within one PostgreSQL major version; major upgrades
  (pg_upgrade) are not supported.
- The credentials Secret reference is fixed once a cluster exists (the managed
  passwords themselves can be rotated — see Rotating credentials).
- Initialisation scripts are SQL only, and run only when a cluster is first
  created.
