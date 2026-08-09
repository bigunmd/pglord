# pglord

Control plane and web UI for HA PostgreSQL clusters on Kubernetes.

pglord runs **namespaced**: it manages Postgres clusters in its own namespace and
never asks for cluster-wide permissions. Installing it twice in two namespaces
gives you two independent installations.

## Prerequisites

- Kubernetes 1.25+
- A default StorageClass (instances use `PersistentVolumeClaim`s). For growing
  storage later, it also needs `allowVolumeExpansion: true`:
  ```bash
  kubectl get storageclass -o custom-columns=NAME:.metadata.name,EXPANSION:.allowVolumeExpansion
  ```
- Two images reachable from your cluster: the server, and the agent image used
  by the Postgres instances.

## Install

```bash
helm install pglord ./charts/pglord \
  --namespace pglord --create-namespace \
  --set image.repository=<registry>/pglord-server \
  --set agentImage=<registry>/pglord-agent:0.1.0
```

Then reach the UI:

```bash
kubectl -n pglord port-forward svc/pglord 8080:80
```

Open <http://localhost:8080> and check the **Permissions** page first. It asks
the API server what pglord is actually allowed to do, so a missing rule appears
as a denied row instead of as a cluster that never starts.

## Before you expose this

**Authentication is off by default.** Until you configure it, anyone who can
reach the Service can create and delete clusters, and deleting a cluster
removes its data volumes. `ingress.enabled=true` publishes it as-is.

Turn one of the `auth.*` modes on before exposing it — static tokens
(`auth.tokens`), OIDC (`auth.oidc`), or Kubernetes tokens (`auth.kubernetes`).
For per-namespace permissions rather than all-or-nothing admin, use
`auth.kubernetes` or `auth.oidc.rbac`; both authorize every request against
Kubernetes RBAC. See the user guide's Authentication section.

## `agentImage` is not optional in practice

The built-in default, `pglord-agent:dev`, exists only on a development machine.
Leave it in place on a real cluster and every cluster you create sits in
`ImagePullBackOff`. Set `agentImage` to something your nodes can pull; it
becomes the image used whenever a cluster is created without naming one.

## Values

| Key | Default | Description |
|---|---|---|
| `image.repository` | `pglord-server` | Server image |
| `image.tag` | `""` | Defaults to the chart's `appVersion` |
| `image.pullPolicy` | `IfNotPresent` | |
| `agentImage` | `pglord-agent:dev` | Image for Postgres instances. **Set this.** |
| `imagePullSecrets` | `[]` | Applied to the server pod |
| `replicaCount` | `1` | See note below |
| `serviceAccount.create` | `true` | |
| `serviceAccount.name` | `""` | Defaults to the release name |
| `rbac.create` | `true` | Namespaced Role and RoleBinding pglord needs |
| `service.type` | `ClusterIP` | |
| `service.port` | `80` | |
| `ingress.enabled` | `false` | Read the warning above first |
| `ingress.className` | `""` | |
| `ingress.hosts` | `pglord.local` | |
| `ingress.tls` | `[]` | |
| `config.logLevel` | `info` | tracing directive, e.g. `pglord_server=debug,info` |
| `config.corsAllowedOrigins` | `""` | Only needed when the UI is served elsewhere |
| `config.shutdownTimeoutSeconds` | `25` | Keep below the termination grace period |
| `resources` | 50m / 32Mi, limit 128Mi | The server holds no cluster data |
| `podSecurityContext` | non-root, seccomp `RuntimeDefault` | |
| `securityContext` | read-only root, no privilege escalation, drop ALL | |
| `nodeSelector`, `tolerations`, `affinity` | `{}` / `[]` | |
| `extraEnv` | `[]` | Extra `PGLORD_*` settings |

### On `replicaCount`

pglord keeps no state of its own — cluster specs live in ConfigMaps in the
namespace — so more than one replica is safe. It also buys nothing today: the
reconciler and the backup scheduler run in every replica, so a second one
duplicates that work rather than sharing it. Leave it at 1 unless you are
testing failure behaviour.

## Upgrade

```bash
helm upgrade pglord ./charts/pglord --namespace pglord --reuse-values \
  --set image.tag=<new-version>
```

Upgrading the server does not touch running Postgres clusters: their specs live
in ConfigMaps, and the reconciler picks up where it left off.

## Uninstall

```bash
helm uninstall pglord --namespace pglord
```

**This does not delete your Postgres clusters.** Their StatefulSets, Services,
Secrets and volumes are owned by the spec ConfigMaps, not by the Helm release,
so they keep running without a control plane. Delete clusters through the UI
first if that is what you meant, or afterwards:

```bash
kubectl -n pglord delete configmap -l app.kubernetes.io/managed-by=pglord
```
