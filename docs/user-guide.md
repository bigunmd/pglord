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

Scheduled backups to object storage are on the roadmap, not shipped yet.

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
- No scheduled backups or point-in-time restore yet.
- Custom images must be built on the pglord agent image; stock `postgres:NN`
  images are not usable yet.
- Cluster name and image cannot be changed after creation (instances and
  storage can — see Day-2 operations).
