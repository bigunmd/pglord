#!/usr/bin/env bash
# Full-stack e2e on the minikube stand: create a 2-instance cluster through the
# REST API, verify streaming replication between the pods the reconciler
# created, then delete and verify garbage collection.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18090

kill_pf() {
  # Killing the minikube wrapper leaves the real kubectl child alive; kill by
  # command line so no stale forward keeps the port bound to a dead pod.
  pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true
}

cleanup() {
  curl -s -X DELETE "$API/api/v1/clusters/demo" >/dev/null 2>&1 || true
  kill_pf
}
trap cleanup EXIT

echo "==> building images"
make image agent-image >/dev/null

echo "==> loading images into minikube"
# minikube keeps cached tags and `image rm` fails while a pod uses the image:
# scale everything down first, then rm + load, then bring the server back.
$KUBECTL apply -f deploy/k8s/namespace.yaml >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=0 >/dev/null 2>&1 || true
$KUBECTL -n pglord wait --for=delete pod -l app=pglord-server --timeout=60s >/dev/null 2>&1 || true
for img in pglord-server:dev pglord-agent:dev; do
  minikube -p pglord image rm "docker.io/library/$img" >/dev/null 2>&1 || true
  minikube -p pglord image load "$img"
done

echo "==> (re)deploying pglord-server"
$KUBECTL apply -f deploy/k8s/ >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=1 >/dev/null
$KUBECTL -n pglord rollout status deploy/pglord-server --timeout=120s >/dev/null

kill_pf
$KUBECTL -n pglord port-forward svc/pglord-server 18090:80 >/dev/null 2>&1 &
sleep 2

echo "==> creating cluster 'demo' via REST API"
code=$(curl -s -o /tmp/create.json -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' -d '{"name":"demo","replicas":2}')
[ "$code" = "201" ] || { echo "FAIL: create returned $code: $(cat /tmp/create.json)"; exit 1; }
echo "OK created"

echo "==> waiting for 2/2 ready (bootstrap + basebackup take a while)"
for _ in $(seq 1 180); do
  ready=$(curl -s "$API/api/v1/clusters/demo" | python3 -c "import sys,json; print(json.load(sys.stdin)['ready_replicas'])" 2>/dev/null || echo "")
  [ "$ready" = "2" ] && break
  sleep 2
done
[ "$ready" = "2" ] || { echo "FAIL: ready_replicas=$ready"; $KUBECTL -n pglord get pods; $KUBECTL -n pglord logs demo-0 --tail 15 || true; exit 1; }
echo "OK cluster ready 2/2"

echo "==> verifying streaming replication (scram credentials from the generated Secret)"
SU_PW=$($KUBECTL -n pglord get secret demo-credentials -o jsonpath='{.data.superuser-password}' | base64 -d)
[ -n "$SU_PW" ] || { echo "FAIL: credentials secret missing"; exit 1; }
$KUBECTL -n pglord exec demo-0 -- env PGPASSWORD="$SU_PW" psql -U postgres -c "CREATE TABLE pglord_e2e(x int); INSERT INTO pglord_e2e VALUES (42);" >/dev/null
for _ in $(seq 1 30); do
  got=$($KUBECTL -n pglord exec demo-1 -- env PGPASSWORD="$SU_PW" psql -U postgres -tAc "SELECT x FROM pglord_e2e" 2>/dev/null || echo "")
  [ "$got" = "42" ] && break
  sleep 1
done
[ "$got" = "42" ] || { echo "FAIL: row did not replicate (got '$got')"; exit 1; }
echo "OK row replicated demo-0 -> demo-1"

echo "==> verifying -rw service points at the primary"
rw_ep=$($KUBECTL -n pglord get endpointslices -l kubernetes.io/service-name=demo-rw -o jsonpath='{.items[0].endpoints[0].targetRef.name}')
[ "$rw_ep" = "demo-0" ] || { echo "FAIL: -rw endpoint is '$rw_ep'"; exit 1; }
echo "OK demo-rw -> demo-0"

echo "==> deleting cluster and verifying garbage collection"
code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$API/api/v1/clusters/demo")
[ "$code" = "204" ] || { echo "FAIL: delete returned $code"; exit 1; }
for _ in $(seq 1 60); do
  if ! $KUBECTL -n pglord get statefulset demo >/dev/null 2>&1; then gone=yes; break; fi
  sleep 1
done
[ "${gone:-}" = "yes" ] || { echo "FAIL: StatefulSet not garbage-collected"; exit 1; }
# PVCs have no ownerReferences; they go away via the StatefulSet retention
# policy. Without this check, orphaned volumes would accumulate unnoticed.
for _ in $(seq 1 60); do
  left=$($KUBECTL -n pglord get pvc -o name 2>/dev/null | grep -c 'data-demo-' || true)
  [ "$left" = "0" ] && break
  sleep 1
done
[ "$left" = "0" ] || { echo "FAIL: $left PVCs left behind after delete"; $KUBECTL -n pglord get pvc; exit 1; }
echo "OK children and volumes garbage-collected"

echo "cluster e2e passed"
