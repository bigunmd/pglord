#!/usr/bin/env bash
# Multi-namespace operator e2e: one cluster-wide operator, same-named clusters
# in two namespaces, namespace-qualified API, per-namespace isolation, GC on
# namespace deletion.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18098

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  $KUBECTL delete ns team-a team-b --ignore-not-found --wait=false >/dev/null 2>&1 || true
  $KUBECTL delete clusterrole pglord-operator --ignore-not-found >/dev/null 2>&1 || true
  $KUBECTL delete clusterrolebinding pglord-operator --ignore-not-found >/dev/null 2>&1 || true
  $KUBECTL -n pglord set env deploy/pglord-server PGLORD_SPEC_BACKEND=configmap PGLORD_ALL_NAMESPACES=false >/dev/null 2>&1 || true
  kill_pf
}
trap cleanup EXIT

jsonq() { python3 -c "import sys,json; d=json.load(sys.stdin); print($1)" 2>/dev/null || echo ""; }
pgc() { # ns jsonpath
  $KUBECTL -n "$1" get pgc demo -o jsonpath="$2" 2>/dev/null || echo ""
}
psql_in() { # ns pod sql pw
  $KUBECTL -n "$1" exec "$2" -- env PGPASSWORD="$4" psql -U postgres -tAc "$3" 2>/dev/null || echo ""
}

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

echo "==> installing CRD + cluster-wide RBAC + operator mode"
$KUBECTL apply -f deploy/crds/ >/dev/null
$KUBECTL apply -f deploy/k8s/ >/dev/null
$KUBECTL apply -f deploy/k8s/operator/rbac.yaml >/dev/null
$KUBECTL -n pglord set env deploy/pglord-server PGLORD_SPEC_BACKEND=crd PGLORD_ALL_NAMESPACES=true >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=1 >/dev/null
$KUBECTL -n pglord rollout status deploy/pglord-server --timeout=120s >/dev/null

kill_pf
$KUBECTL -n pglord port-forward svc/pglord-server 18098:80 >/dev/null 2>&1 &
sleep 2

echo "==> same-named cluster 'demo' in team-a and team-b"
for ns in team-a team-b; do
  $KUBECTL create ns "$ns" >/dev/null
  $KUBECTL -n "$ns" apply -f - >/dev/null <<'EOF'
apiVersion: pglord.dev/v1alpha1
kind: PostgresCluster
metadata:
  name: demo
spec:
  replicas: 1
EOF
done
for ns in team-a team-b; do
  phase=""
  for _ in $(seq 1 180); do
    phase=$(pgc "$ns" '{.status.phase}')
    [ "$phase" = "Ready" ] && break
    sleep 2
  done
  [ "$phase" = "Ready" ] || { echo "FAIL: $ns/demo phase=$phase"; exit 1; }
done
echo "OK both Ready"

echo "==> distinct data per namespace"
PW_A=$($KUBECTL -n team-a get secret demo-credentials -o jsonpath='{.data.superuser-password}' | base64 -d)
PW_B=$($KUBECTL -n team-b get secret demo-credentials -o jsonpath='{.data.superuser-password}' | base64 -d)
psql_in team-a demo-0 "CREATE TABLE t(x text); INSERT INTO t VALUES ('alpha');" "$PW_A" >/dev/null
psql_in team-b demo-0 "CREATE TABLE t(x text); INSERT INTO t VALUES ('bravo');" "$PW_B" >/dev/null
[ "$PW_A" != "$PW_B" ] || { echo "FAIL: credentials shared across namespaces"; exit 1; }
echo "OK separate credentials and data"

echo "==> global list shows both, with namespaces"
rows=$(curl -s "$API/api/v1/clusters" | jsonq "sorted((c['namespace'], c['name']) for c in d)")
echo "$rows" | grep -q "team-a" && echo "$rows" | grep -q "team-b" \
  || { echo "FAIL: global list missing namespaces: $rows"; exit 1; }
echo "OK global list"

echo "==> ns-qualified REST edit converges only in team-a"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/namespaces/team-a/clusters/demo" \
  -H 'content-type: application/json' -d '{"parameters":{"max_connections":"150"}}')
[ "$code" = "200" ] || { echo "FAIL: ns-qualified PATCH returned $code"; exit 1; }
mc_a=""
for _ in $(seq 1 30); do
  mc_a=$(pgc team-a '{.spec.parameters.max_connections}')
  [ "$mc_a" = "150" ] && break
  sleep 2
done
mc_b=$(pgc team-b '{.spec.parameters.max_connections}')
[ "$mc_a" = "150" ] && [ -z "$mc_b" ] || { echo "FAIL: edit leaked (a=$mc_a b=$mc_b)"; exit 1; }
echo "OK edit isolated to team-a"

echo "==> legacy alias stays scoped to the operator namespace"
code=$(curl -s -o /dev/null -w '%{http_code}' "$API/api/v1/clusters/demo")
[ "$code" = "404" ] || { echo "FAIL: legacy path found a foreign-ns cluster ($code)"; exit 1; }
echo "OK legacy alias own-ns only"

echo "==> deleting namespace team-b garbage-collects only its cluster"
$KUBECTL delete ns team-b --wait=false >/dev/null
gone=""
for _ in $(seq 1 120); do
  left=$($KUBECTL -n team-b get pvc --no-headers 2>/dev/null | grep -c 'demo-' || true)
  ns_phase=$($KUBECTL get ns team-b -o jsonpath='{.status.phase}' 2>/dev/null || true)
  [ -z "$ns_phase" ] && gone=yes && break
  sleep 2
done
[ "$gone" = "yes" ] || { echo "FAIL: team-b namespace never finished deleting"; exit 1; }
val=$(psql_in team-a demo-0 "SELECT x FROM t" "$PW_A")
[ "$val" = "alpha" ] || { echo "FAIL: team-a data affected by team-b deletion (got '$val')"; exit 1; }
phase=$(pgc team-a '{.status.phase}')
[ "$phase" = "Ready" ] || { echo "FAIL: team-a degraded after team-b deletion (phase=$phase)"; exit 1; }
echo "OK isolation held through namespace deletion"

echo "PASS: multi-namespace operator e2e"
