# pglord user guide

pglord runs highly-available PostgreSQL clusters in your Kubernetes namespace.
You create a cluster from a web UI; pglord provisions the instances, keeps one
primary and one or more streaming standbys, and promotes a standby automatically
if the primary fails.

Everything lives in a single namespace. You do not need cluster-admin.

---

## 1. Install

You need: access to a Kubernetes namespace, `kubectl`, and the two pglord images
(`pglord-server`, `pglord-agent`) available to your cluster — from your registry,
or built locally with `make image agent-image` for local clusters like minikube.

```bash
kubectl apply -f deploy/k8s/
kubectl -n pglord rollout status deploy/pglord-server
```

This creates the `pglord` namespace, a ServiceAccount with a namespace-scoped
Role, the pglord Deployment, and its Service. To install into a different
namespace, change `namespace:` in the manifests.

Open the UI:

```bash
kubectl -n pglord port-forward svc/pglord-server 8080:80
# http://localhost:8080
```

For shared access, expose `svc/pglord-server` through your Ingress controller
instead of port-forwarding. pglord has no built-in authentication yet — put it
behind your ingress auth, and do not expose it publicly.

## 2. Check permissions

Open **Permissions** in the UI. It asks the Kubernetes API server what pglord is
actually allowed to do and lists every required permission as granted, denied, or
unknown.

If anything is denied, cluster management will fail. Ask whoever administers the
namespace to apply `deploy/k8s/rbac.yaml`, then reload the page. The same report
is available at `GET /api/v1/preflight`, and the server logs a summary at
startup.

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

Uncommitted transactions on the old primary are lost. This is asynchronous
replication: a small window of recently-committed writes can be lost if the
primary fails before the standby receives them.

## 6. Day-2 operations

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

**Restore is not available yet.** A backup you cannot restore is not yet a
safety net, so until that ships, take a `pg_dump` before anything irreversible
(see *Delete a cluster*). Scheduled backups are also still to come; for now a
backup happens when you ask for one.

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

Editing `pg_hba.conf` is not available yet; connection rules stay as pglord
configures them.

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

## 7. Delete a cluster

**Clusters → trash icon → Delete.** This removes the instances, their persistent
volumes, and the generated credentials. It cannot be undone; take a dump first if
you need the data:

```bash
kubectl -n pglord exec <name>-0 -- \
  env PGPASSWORD=<password> pg_dump -U postgres postgres > backup.sql
```

A repository backup is not a substitute here: restoring from one is not
supported yet, so a dump is the only way to get this data back.

## 8. Troubleshooting

| Symptom | Likely cause and fix |
|---|---|
| Permissions page shows denied entries | The namespace Role is missing rules. Apply `deploy/k8s/rbac.yaml`. |
| Instances stay at 0 ready, pods `Pending` | No storage available. Check `kubectl -n pglord get pvc` and whether your cluster has a default StorageClass. |
| Instances stay at 0 ready, pods `ImagePullBackOff` | The agent image is not reachable from the cluster. Push it to your registry and set that image when creating the cluster. |
| An instance shows role "Unknown" | Its agent is unreachable. Check `kubectl -n pglord logs <name>-<n>`. |
| Cluster list shows "No Kubernetes environment" | The server has no cluster access — usually a missing ServiceAccount on the Deployment. |
| A standby never becomes ready | It is still cloning, or cannot reach the primary. Check its logs for `pg_basebackup`. |

## Current limitations

- No authentication in front of the UI — put it behind your ingress auth.
- Asynchronous replication only (synchronous is not exposed yet).
- Backups can be taken and listed, but **not restored** yet, and cannot be
  scheduled — see *Back up to object storage*.
- Custom images must be built on the pglord agent image; stock `postgres:NN`
  images are not usable yet.
- Cluster name and image cannot be changed after creation (instances, storage
  and PostgreSQL settings can — see Day-2 operations).
- `pg_hba.conf` rules are not editable yet.
