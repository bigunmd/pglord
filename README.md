# pglord

Control plane for HA PostgreSQL clusters on Kubernetes: deploy, control, and monitor
highly-available Postgres from a web UI or REST API — standalone in your own namespace
(no cluster-admin required) or, later, as a cluster operator.

Status: early foundation. See `docs/superpowers/specs/` for the design and
`docs/superpowers/plans/` for implementation plans.

## Deploy

pglord runs standalone in a single namespace — no CRDs, no cluster-admin. What you need:
a Kubernetes cluster, `kubectl` access, and the two images available to the cluster.

```bash
# 1. Build the images
make image agent-image        # pglord-server:dev (UI embedded), pglord-agent:dev

# 2. Make them available to your cluster
#    minikube:  make stand-up stand-deploy   (does load + apply for you)
#    other clusters: push both images to your registry and adjust
#    deploy/k8s/deployment.yaml (server) and the cluster spec `image` field (agent)

# 3. Apply the manifests (namespace, namespaced RBAC, Deployment, Service)
kubectl apply -f deploy/k8s/

# 4. Open the UI
kubectl -n pglord port-forward svc/pglord-server 8080:80   # http://localhost:8080
```

The server's ServiceAccount only has a namespaced Role (`deploy/k8s/rbac.yaml`):
ConfigMaps, Secrets, Services, Pods, ServiceAccounts, StatefulSets, and
Roles/RoleBindings — everything stays inside the `pglord` namespace.

Clusters created via UI/API store their spec as a `<name>-spec` ConfigMap; all
derived objects (StatefulSet, Services, credentials Secret, instance RBAC) carry
ownerReferences to it, so deleting a cluster garbage-collects everything.

## Development

```bash
make help          # list all targets
make run           # run the server locally (http://localhost:8080)
make test          # run all tests
make lint          # fmt --check + clippy -D warnings
make openapi       # regenerate spec/openapi.yaml
make stand-up      # start the minikube dev stand
make stand-deploy  # build image, load into minikube, deploy
make stand-smoke   # curl healthz/readyz/ping through a port-forward
```

## Configuration

Layered: serde defaults ← `config.toml` ← `PGLORD_*` environment variables.

| Variable | Default | Description |
|---|---|---|
| `PGLORD_HTTP_ADDR` | `0.0.0.0:8080` | HTTP listen address |
| `PGLORD_LOG_LEVEL` | `info` | tracing EnvFilter directive |
| `PGLORD_SHUTDOWN_TIMEOUT_SECS` | `25` | graceful drain budget (< k8s 30s grace) |
| `PGLORD_CORS_ALLOWED_ORIGINS` | *(empty)* | comma-separated exact origins; empty disables CORS |

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).
