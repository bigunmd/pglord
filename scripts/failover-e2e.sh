#!/usr/bin/env bash
# Failover e2e on the minikube stand: sustained primary failure (cordon node +
# delete pod) -> orchestrator elects the standby, CAS-flips the record, -rw
# repoints -> uncordon -> old primary rejoins as standby via pg_rewind.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18091
NODE=pglord

cleanup() {
  $KUBECTL uncordon "$NODE" >/dev/null 2>&1 || true
  kill "${PF:-}" 2>/dev/null || true
  curl -s -X DELETE "$API/api/v1/clusters/demo" >/dev/null 2>&1 || true
}
trap cleanup EXIT

psql_on() { # pod sql
  $KUBECTL -n pglord exec "$1" -- env PGPASSWORD="$SU_PW" psql -U postgres -tAc "$2" 2>/dev/null || echo ""
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
$KUBECTL apply -f deploy/k8s/ >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=1 >/dev/null
$KUBECTL -n pglord rollout status deploy/pglord-server --timeout=120s >/dev/null

$KUBECTL -n pglord port-forward svc/pglord-server 18091:80 >/dev/null 2>&1 &
PF=$!
sleep 2

echo "==> creating cluster 'demo' (replicas=2)"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' -d '{"name":"demo","replicas":2}')
[ "$code" = "201" ] || { echo "FAIL: create returned $code"; exit 1; }
for _ in $(seq 1 180); do
  ready=$(curl -s "$API/api/v1/clusters/demo" | python3 -c "import sys,json; print(json.load(sys.stdin)['ready_replicas'])" 2>/dev/null || echo "")
  [ "$ready" = "2" ] && break
  sleep 2
done
[ "$ready" = "2" ] || { echo "FAIL: cluster never reached 2/2"; exit 1; }
SU_PW=$($KUBECTL -n pglord get secret demo-credentials -o jsonpath='{.data.superuser-password}' | base64 -d)
echo "OK cluster ready"

echo "==> writing marker row on primary"
psql_on demo-0 "CREATE TABLE failover_e2e(x int); INSERT INTO failover_e2e VALUES (42);" >/dev/null
sleep 3 # let it replicate

echo "==> killing primary (cordon node so demo-0 stays gone)"
$KUBECTL cordon "$NODE" >/dev/null
$KUBECTL -n pglord delete pod demo-0 --wait=false >/dev/null

echo "==> waiting for failover to demo-1"
for _ in $(seq 1 60); do
  rec=$($KUBECTL -n pglord get cm demo-spec -o jsonpath='{.metadata.annotations.pglord\.dev/primary}' 2>/dev/null || echo "")
  in_rec=$(psql_on demo-1 "SELECT pg_is_in_recovery()")
  [ "$rec" = "demo-1" ] && [ "$in_rec" = "f" ] && break
  sleep 2
done
[ "$rec" = "demo-1" ] || { echo "FAIL: record not flipped (rec='$rec')"; exit 1; }
[ "$in_rec" = "f" ] || { echo "FAIL: demo-1 not promoted"; exit 1; }
echo "OK failover: record=demo-1, demo-1 promoted"

echo "==> verifying -rw repointed to demo-1"
for _ in $(seq 1 30); do
  rw_ep=$($KUBECTL -n pglord get endpointslices -l kubernetes.io/service-name=demo-rw -o jsonpath='{.items[0].endpoints[0].targetRef.name}' 2>/dev/null || echo "")
  [ "$rw_ep" = "demo-1" ] && break
  sleep 2
done
[ "$rw_ep" = "demo-1" ] || { echo "FAIL: -rw endpoint is '$rw_ep'"; exit 1; }
echo "OK demo-rw -> demo-1"

echo "==> marker row survived on new primary"
got=$(psql_on demo-1 "SELECT x FROM failover_e2e")
[ "$got" = "42" ] || { echo "FAIL: marker row lost (got '$got')"; exit 1; }
echo "OK data intact"

echo "==> uncordon; old primary must rejoin as standby (pg_rewind)"
$KUBECTL uncordon "$NODE" >/dev/null
for _ in $(seq 1 120); do
  in_rec0=$(psql_on demo-0 "SELECT pg_is_in_recovery()")
  [ "$in_rec0" = "t" ] && break
  sleep 2
done
[ "$in_rec0" = "t" ] || { echo "FAIL: demo-0 did not rejoin as standby"; $KUBECTL -n pglord logs demo-0 --tail 20 || true; exit 1; }
echo "OK demo-0 rejoined as standby"

echo "==> write through demo-rw replicates to rejoined standby"
$KUBECTL -n pglord exec demo-1 -- env PGPASSWORD="$SU_PW" psql -h demo-rw -U postgres -c "INSERT INTO failover_e2e VALUES (43);" >/dev/null
for _ in $(seq 1 30); do
  got=$(psql_on demo-0 "SELECT count(*) FROM failover_e2e")
  [ "$got" = "2" ] && break
  sleep 1
done
[ "$got" = "2" ] || { echo "FAIL: write did not replicate to rejoined standby"; exit 1; }
echo "OK rw write replicated demo-1 -> demo-0"

echo "==> cleanup"
curl -s -X DELETE "$API/api/v1/clusters/demo" >/dev/null
echo "failover e2e passed"
