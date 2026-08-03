# Cluster-wide operator install

1. `kubectl apply -f deploy/crds/` — install the PostgresCluster CRD.
2. `kubectl apply -f deploy/k8s/` — base ServiceAccount, Deployment, Service.
3. `kubectl apply -f deploy/k8s/operator/rbac.yaml` — ClusterRole instead of
   the namespaced Role.
4. Set `PGLORD_SPEC_BACKEND=crd` and `PGLORD_ALL_NAMESPACES=true` on the
   pglord-server Deployment.
