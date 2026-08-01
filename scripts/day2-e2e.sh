#!/usr/bin/env bash
# Day-2 operations e2e: planned switchover to a chosen instance, then scale up.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18095

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  curl -s -X DELETE "$API/api/v1/clusters/demo" >/dev/null 2>&1 || true
  kill_pf
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

kill_pf
$KUBECTL -n pglord port-forward svc/pglord-server 18095:80 >/dev/null 2>&1 &
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
psql_on demo-0 "CREATE TABLE day2(x int); INSERT INTO day2 VALUES (7);" >/dev/null
sleep 3
echo "OK cluster ready"

echo "==> rejecting invalid switchover targets"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/demo/switchover" \
  -H 'content-type: application/json' -d '{"target":"demo-9"}')
[ "$code" = "422" ] || { echo "FAIL: bad target returned $code, expected 422"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/demo/switchover" \
  -H 'content-type: application/json' -d '{"target":"demo-0"}')
[ "$code" = "409" ] || { echo "FAIL: current primary returned $code, expected 409"; exit 1; }
echo "OK invalid targets rejected"

echo "==> switching over to demo-1"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/demo/switchover" \
  -H 'content-type: application/json' -d '{"target":"demo-1"}')
[ "$code" = "202" ] || { echo "FAIL: switchover returned $code"; exit 1; }
for _ in $(seq 1 60); do
  rec=$($KUBECTL -n pglord get cm demo-spec -o jsonpath='{.metadata.annotations.pglord\.dev/primary}' 2>/dev/null || echo "")
  in_rec=$(psql_on demo-1 "SELECT pg_is_in_recovery()")
  [ "$rec" = "demo-1" ] && [ "$in_rec" = "f" ] && break
  sleep 2
done
[ "$rec" = "demo-1" ] && [ "$in_rec" = "f" ] || { echo "FAIL: switchover incomplete (rec=$rec in_recovery=$in_rec)"; exit 1; }
echo "OK demo-1 is now primary"

for _ in $(seq 1 30); do
  rw_ep=$($KUBECTL -n pglord get endpointslices -l kubernetes.io/service-name=demo-rw -o jsonpath='{.items[0].endpoints[0].targetRef.name}' 2>/dev/null || echo "")
  [ "$rw_ep" = "demo-1" ] && break
  sleep 2
done
[ "$rw_ep" = "demo-1" ] || { echo "FAIL: -rw endpoint is '$rw_ep'"; exit 1; }
got=$(psql_on demo-1 "SELECT x FROM day2")
[ "$got" = "7" ] || { echo "FAIL: data missing after switchover (got '$got')"; exit 1; }
echo "OK -rw repointed and data intact"

echo "==> rejecting a storage shrink"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/demo" \
  -H 'content-type: application/json' -d '{"storage_size":"500Mi"}')
[ "$code" = "422" ] || { echo "FAIL: shrink returned $code, expected 422"; exit 1; }
echo "OK shrink rejected"

echo "==> growing storage"
# minikube's default StorageClass does not allow expansion, and the API server
# refuses to resize a claim whose class does not. Turn it on for the stand: the
# hostpath provisioner still will not really grow a filesystem, so what this
# asserts is that pglord patches the claim -- which is pglord's job, and the
# part that needs `patch` on persistentvolumeclaims. The actual resize belongs
# to the storage driver.
$KUBECTL patch storageclass standard -p '{"allowVolumeExpansion":true}' >/dev/null 2>&1 || true

# Recorded before the growth so we can tell afterwards whether the pod was
# replaced. Growing a disk must never restart the database.
uid_before=$($KUBECTL -n pglord get pod demo-1 -o jsonpath='{.metadata.uid}' 2>/dev/null || echo "")

# The shrink case never reaches Kubernetes, so it proves nothing about
# permissions. Growth patches the claims themselves, which needs `patch` on
# persistentvolumeclaims -- a rule that was missing from the Role and from the
# permission preflight, so this failed with a 403 nothing warned about.
code=$(curl -s -o /tmp/grow.json -w '%{http_code}' -X PATCH "$API/api/v1/clusters/demo" \
  -H 'content-type: application/json' -d '{"storage_size":"2Gi"}')
[ "$code" = "200" ] || { echo "FAIL: grow returned $code: $(cat /tmp/grow.json)"; exit 1; }
for _ in $(seq 1 60); do
  grown=$($KUBECTL -n pglord get pvc data-demo-0 -o jsonpath='{.spec.resources.requests.storage}' 2>/dev/null || echo "")
  [ "$grown" = "2Gi" ] && break
  sleep 2
done
[ "$grown" = "2Gi" ] || {
  echo "FAIL: claim still requests '$grown', expected 2Gi"
  $KUBECTL -n pglord get pvc
  $KUBECTL -n pglord logs deploy/pglord-server --tail 20
  exit 1
}
echo "OK storage grew on the claim"

echo "==> the StatefulSet picks up the new claim template without disturbing anything"
# volumeClaimTemplates are immutable, so pglord recreates the StatefulSet with
# orphan propagation. That has to leave the pods running and the volumes alone:
# get it wrong and growing storage destroys the database.
for _ in $(seq 1 60); do
  tmpl=$($KUBECTL -n pglord get sts demo -o jsonpath='{.spec.volumeClaimTemplates[0].spec.resources.requests.storage}' 2>/dev/null || echo "")
  [ "$tmpl" = "2Gi" ] && break
  sleep 2
done
[ "$tmpl" = "2Gi" ] || { echo "FAIL: claim template is '$tmpl', expected 2Gi"; $KUBECTL -n pglord logs deploy/pglord-server --tail 20; exit 1; }
uid_after=$($KUBECTL -n pglord get pod demo-1 -o jsonpath='{.metadata.uid}' 2>/dev/null || echo "")
[ -n "$uid_before" ] && [ "$uid_after" = "$uid_before" ] || {
  echo "FAIL: primary pod was replaced ('$uid_before' -> '$uid_after'); the recreate restarted the database"
  exit 1
}
got=$(psql_on demo-1 "SELECT x FROM day2")
[ "$got" = "7" ] || { echo "FAIL: data lost across the StatefulSet recreate (got '$got')"; exit 1; }
echo "OK StatefulSet recreated in place: same pod, data intact"

echo "==> the permission preflight knows about claims"
# If PVC verbs are absent from the required list, a namespace missing them
# reports "all good" right up until a day-2 operation fails.
pvc_checks=$(curl -s "$API/api/v1/preflight" | python3 -c "
import sys,json
d=json.load(sys.stdin)
print(sum(1 for c in d['checks'] if c['resource'].startswith('persistentvolumeclaims')))")
[ "${pvc_checks:-0}" -ge 2 ] || { echo "FAIL: preflight reports $pvc_checks claim checks"; exit 1; }
echo "OK preflight covers claims ($pvc_checks checks)"

echo "==> scaling to 3 instances"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/demo" \
  -H 'content-type: application/json' -d '{"replicas":3}')
[ "$code" = "200" ] || { echo "FAIL: scale returned $code"; exit 1; }
for _ in $(seq 1 120); do
  ready=$(curl -s "$API/api/v1/clusters/demo" | python3 -c "import sys,json; print(json.load(sys.stdin)['ready_replicas'])" 2>/dev/null || echo "")
  [ "$ready" = "3" ] && break
  sleep 2
done
[ "$ready" = "3" ] || { echo "FAIL: never reached 3/3 (at $ready)"; $KUBECTL -n pglord get pods; exit 1; }
got=$(psql_on demo-2 "SELECT x FROM day2")
[ "$got" = "7" ] || { echo "FAIL: new instance did not clone data (got '$got')"; exit 1; }
echo "OK scaled to 3/3 and demo-2 streaming from the new primary"

echo "==> cleanup"
curl -s -X DELETE "$API/api/v1/clusters/demo" >/dev/null
echo "day2 e2e passed"
