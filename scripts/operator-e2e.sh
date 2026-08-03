#!/usr/bin/env bash
# Operator-mode e2e: PostgresCluster CRD as the spec store. kubectl create/
# edit converge, REST edits land on the CR, status/printer columns fill,
# failover and hibernation work through the mirror, adoption guard holds,
# deleting the CR garbage-collects everything.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18097

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  $KUBECTL uncordon pglord >/dev/null 2>&1 || true
  $KUBECTL -n pglord delete pgc crdemo other --ignore-not-found >/dev/null 2>&1 || true
  $KUBECTL -n pglord delete cm other-spec --ignore-not-found >/dev/null 2>&1 || true
  # Back to standalone so later e2e suites see the stand they expect.
  $KUBECTL -n pglord set env deploy/pglord-server PGLORD_SPEC_BACKEND=configmap >/dev/null 2>&1 || true
  kill_pf
}
trap cleanup EXIT

detail() { curl -s "$API/api/v1/clusters/crdemo"; }
jsonq() { python3 -c "import sys,json; d=json.load(sys.stdin); print($1)" 2>/dev/null || echo ""; }
pgc() { $KUBECTL -n pglord get pgc crdemo -o jsonpath="$1" 2>/dev/null || echo ""; }

echo "==> building + loading images"
make image agent-image >/dev/null
$KUBECTL apply -f deploy/k8s/namespace.yaml >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=0 >/dev/null 2>&1 || true
$KUBECTL -n pglord wait --for=delete pod -l app=pglord-server --timeout=60s >/dev/null 2>&1 || true
for img in pglord-server:dev pglord-agent:dev; do
  minikube -p pglord image rm "docker.io/library/$img" >/dev/null 2>&1 || true
  minikube -p pglord image load "$img"
done
for img in pglord-server:dev pglord-agent:dev; do
  want=$(docker image inspect "$img" -f '{{.Id}}')
  have=$(minikube -p pglord ssh -- docker image inspect -f '{{.Id}}' "docker.io/library/$img" 2>/dev/null | tr -d '\r')
  [ "$want" = "$have" ] || { echo "FAIL: minikube kept a stale $img (have $have, want $want) — delete leftover clusters and retry"; exit 1; }
done

echo "==> installing CRD + operator RBAC + crd backend"
$KUBECTL apply -f deploy/crds/ >/dev/null
$KUBECTL apply -f deploy/k8s/ >/dev/null
# The raw rbac.yaml ships the pglord.dev rules commented; grant them here.
$KUBECTL -n pglord apply -f - >/dev/null <<'EOF'
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: pglord-server-crd
  namespace: pglord
rules:
  - apiGroups: [pglord.dev]
    resources: [postgresclusters]
    verbs: [get, list, watch, create, patch, delete]
  - apiGroups: [pglord.dev]
    resources: [postgresclusters/status]
    verbs: [get, patch]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: pglord-server-crd
  namespace: pglord
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: pglord-server-crd
subjects:
  - kind: ServiceAccount
    name: pglord-server
    namespace: pglord
EOF
$KUBECTL -n pglord set env deploy/pglord-server PGLORD_SPEC_BACKEND=crd >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=1 >/dev/null
$KUBECTL -n pglord rollout status deploy/pglord-server --timeout=120s >/dev/null

kill_pf
$KUBECTL -n pglord port-forward svc/pglord-server 18097:80 >/dev/null 2>&1 &
sleep 2

echo "==> kubectl-creating PostgresCluster 'crdemo' (no spec.name — injected)"
$KUBECTL -n pglord apply -f - >/dev/null <<'EOF'
apiVersion: pglord.dev/v1alpha1
kind: PostgresCluster
metadata:
  name: crdemo
spec:
  replicas: 2
EOF
ready=""
for _ in $(seq 1 180); do
  ready=$(detail | jsonq "d['ready_replicas']")
  [ "$ready" = "2" ] && break
  sleep 2
done
[ "$ready" = "2" ] || { echo "FAIL: cluster never reached 2/2 via API (ready=$ready)"; exit 1; }
phase=""
for _ in $(seq 1 60); do
  phase=$(pgc '{.status.phase}')
  [ "$phase" = "Ready" ] && break
  sleep 2
done
[ "$phase" = "Ready" ] || { echo "FAIL: status.phase=$phase, expected Ready"; exit 1; }
sr=$(pgc '{.status.ready_replicas}')
[ "$sr" = "2" ] || { echo "FAIL: status.ready_replicas=$sr"; exit 1; }
SU_PW=$($KUBECTL -n pglord get secret crdemo-credentials -o jsonpath='{.data.superuser-password}' | base64 -d)
$KUBECTL -n pglord exec crdemo-0 -- env PGPASSWORD="$SU_PW" psql -U postgres -tAc \
  "CREATE TABLE op(x int); INSERT INTO op VALUES (7);" >/dev/null
echo "OK crdemo Ready via kubectl create, name injected, status populated"

echo "==> kubectl scale to 3 converges"
$KUBECTL -n pglord patch pgc crdemo --type=merge -p '{"spec":{"replicas":3}}' >/dev/null
ready=""
for _ in $(seq 1 180); do
  ready=$(detail | jsonq "d['ready_replicas']")
  [ "$ready" = "3" ] && break
  sleep 2
done
[ "$ready" = "3" ] || { echo "FAIL: kubectl scale never converged (ready=$ready)"; exit 1; }
echo "OK kubectl edit converged"

echo "==> REST edit lands on the CR"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/crdemo" \
  -H 'content-type: application/json' -d '{"parameters":{"max_connections":"150"}}')
[ "$code" = "200" ] || { echo "FAIL: PATCH returned $code"; exit 1; }
mc=""
for _ in $(seq 1 30); do
  mc=$(pgc '{.spec.parameters.max_connections}')
  [ "$mc" = "150" ] && break
  sleep 2
done
[ "$mc" = "150" ] || { echo "FAIL: REST edit not visible on the CR (max_connections=$mc)"; exit 1; }
echo "OK REST edit visible in kubectl"

echo "==> failover updates status.primary (cordon so the failure is sustained)"
# A bare pod delete recovers inside the failover threshold and correctly
# produces no failover — cordon first, exactly like failover-e2e.
old_primary=$(pgc '{.status.primary}')
[ -n "$old_primary" ] || { echo "FAIL: no status.primary"; exit 1; }
NODE=pglord
$KUBECTL cordon "$NODE" >/dev/null
$KUBECTL -n pglord delete pod "$old_primary" --wait=false >/dev/null
new_primary=""
for _ in $(seq 1 90); do
  new_primary=$(pgc '{.status.primary}')
  [ -n "$new_primary" ] && [ "$new_primary" != "$old_primary" ] && break
  sleep 2
done
$KUBECTL uncordon "$NODE" >/dev/null
[ -n "$new_primary" ] && [ "$new_primary" != "$old_primary" ] || { echo "FAIL: primary never moved off $old_primary"; exit 1; }
for _ in $(seq 1 180); do
  ready=$(detail | jsonq "d['ready_replicas']")
  [ "$ready" = "3" ] && break
  sleep 2
done
[ "$ready" = "3" ] || { echo "FAIL: cluster never recovered after failover"; exit 1; }
echo "OK failover: $old_primary -> $new_primary"

echo "==> hibernate via API shows in status.phase"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/crdemo/hibernate")
[ "$code" = "202" ] || { echo "FAIL: hibernate returned $code"; exit 1; }
phase=""
for _ in $(seq 1 90); do
  phase=$(pgc '{.status.phase}')
  pods=$($KUBECTL -n pglord get pods -l app.kubernetes.io/instance=crdemo --no-headers 2>/dev/null | wc -l | tr -d ' ')
  [ "$phase" = "Hibernated" ] && [ "$pods" = "0" ] && break
  sleep 2
done
[ "$phase" = "Hibernated" ] && [ "$pods" = "0" ] || { echo "FAIL: phase=$phase pods=$pods"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/crdemo/wake")
[ "$code" = "202" ] || { echo "FAIL: wake returned $code"; exit 1; }
for _ in $(seq 1 180); do
  ready=$(detail | jsonq "d['ready_replicas']")
  [ "$ready" = "3" ] && break
  sleep 2
done
[ "$ready" = "3" ] || { echo "FAIL: never woke to 3/3"; exit 1; }
primary=$(pgc '{.status.primary}')
val=$($KUBECTL -n pglord exec "$primary" -- env PGPASSWORD="$SU_PW" psql -U postgres -tAc "SELECT x FROM op" 2>/dev/null || echo "")
[ "$val" = "7" ] || { echo "FAIL: marker row lost (got '$val')"; exit 1; }
echo "OK hibernate/wake through the CR status, data intact"

echo "==> adoption guard: a CR cannot clobber a foreign spec ConfigMap"
$KUBECTL -n pglord create cm other-spec >/dev/null
$KUBECTL -n pglord label cm other-spec app.kubernetes.io/managed-by=pglord pglord.dev/kind=cluster-spec >/dev/null
$KUBECTL -n pglord apply -f - >/dev/null <<'EOF'
apiVersion: pglord.dev/v1alpha1
kind: PostgresCluster
metadata:
  name: other
spec:
  replicas: 1
EOF
msg=""
for _ in $(seq 1 60); do
  msg=$($KUBECTL -n pglord get pgc other -o jsonpath='{.status.phase}' 2>/dev/null || echo "")
  [ "$msg" = "Invalid" ] && break
  sleep 2
done
[ "$msg" = "Invalid" ] || { echo "FAIL: conflicting CR phase=$msg, expected Invalid"; exit 1; }
$KUBECTL -n pglord delete pgc other >/dev/null
$KUBECTL -n pglord delete cm other-spec >/dev/null
echo "OK adoption guard holds"

echo "==> kubectl delete garbage-collects the whole chain"
$KUBECTL -n pglord delete pgc crdemo >/dev/null
gone=""
for _ in $(seq 1 120); do
  left_pvc=$($KUBECTL -n pglord get pvc --no-headers 2>/dev/null | grep -c 'crdemo-' || true)
  # get-by-name exits 1 on NotFound; under pipefail that would kill the
  # script exactly when GC succeeds — the || true is what keeps us alive.
  left_cm=$($KUBECTL -n pglord get cm crdemo-spec --no-headers 2>/dev/null | wc -l | tr -d ' ' || true)
  left_sts=$($KUBECTL -n pglord get sts crdemo --no-headers 2>/dev/null | wc -l | tr -d ' ' || true)
  [ "$left_pvc" = "0" ] && [ "$left_cm" = "0" ] && [ "$left_sts" = "0" ] && gone=yes && break
  sleep 2
done
[ "$gone" = "yes" ] || { echo "FAIL: children survived CR deletion (pvc=$left_pvc cm=$left_cm sts=$left_sts)"; exit 1; }
echo "OK CR deletion cascaded"

echo "PASS: operator mode e2e"
